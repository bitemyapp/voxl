/* GBM capture targets and NVENC OpenGL encoding. All calls stay on the owning
 * thread. Wayland ready and NVENC bitstream completion delimit buffer ownership. */
#define _GNU_SOURCE
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ffnvcodec/dynlink_loader.h>

#define TARGETS 2

typedef struct {
    int fd[4];
    uint32_t stride[4], offset[4];
    uint64_t modifier;
    uint32_t planes, width, height, format;
} VoxDmaBuf;

typedef struct {
    int fd;
    struct gbm_device *gbm;
    EGLDisplay display;
    EGLContext context;
    struct gbm_bo *bo[TARGETS];
    EGLImage image[TARGETS];
    GLuint texture[TARGETS], framebuffer[TARGETS];
    GLuint encode_texture[TARGETS], encode_framebuffer[TARGETS];
    uint32_t width,height;
    NvencFunctions *loader;
    NV_ENCODE_API_FUNCTION_LIST nv;
    void *encoder;
    NV_ENC_REGISTERED_PTR registered[TARGETS];
    NV_ENC_OUTPUT_PTR bitstream;
    NV_ENC_INITIALIZE_PARAMS init;
    NV_ENC_CONFIG config;
    NV_ENC_INPUT_RESOURCE_OPENGL_TEX resource[TARGETS];
    uint64_t pts;
    uint32_t kbps;
    float quality;
} VoxGpu;

void voxl_gpu_free(VoxGpu *g) {
    if (!g) return;
    if (g->display!=EGL_NO_DISPLAY && g->context!=EGL_NO_CONTEXT)
        eglMakeCurrent(g->display,EGL_NO_SURFACE,EGL_NO_SURFACE,g->context);
    if (g->encoder) {
        for (int i=0;i<TARGETS;i++) if (g->registered[i]) g->nv.nvEncUnregisterResource(g->encoder,g->registered[i]);
        if (g->bitstream) g->nv.nvEncDestroyBitstreamBuffer(g->encoder,g->bitstream);
        g->nv.nvEncDestroyEncoder(g->encoder);
    }
    nvenc_free_functions(&g->loader);
    for (int i=0;i<TARGETS;i++) {
        if (g->framebuffer[i]) glDeleteFramebuffers(1,&g->framebuffer[i]);
        if (g->texture[i]) glDeleteTextures(1,&g->texture[i]);
        if (g->encode_framebuffer[i]) glDeleteFramebuffers(1,&g->encode_framebuffer[i]);
        if (g->encode_texture[i]) glDeleteTextures(1,&g->encode_texture[i]);
        if (g->image[i]!=EGL_NO_IMAGE) eglDestroyImage(g->display,g->image[i]);
        if (g->bo[i]) gbm_bo_destroy(g->bo[i]);
    }
    if (g->display!=EGL_NO_DISPLAY) {
        eglMakeCurrent(g->display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
        if (g->context!=EGL_NO_CONTEXT) eglDestroyContext(g->display,g->context);
        eglTerminate(g->display);
    }
    if (g->gbm) gbm_device_destroy(g->gbm);
    if (g->fd>=0) close(g->fd);
    free(g);
}

static int nvcheck(VoxGpu *g,NVENCSTATUS status,char *error,size_t cap,const char *where) {
    if (status==NV_ENC_SUCCESS) return 0;
    const char *why=g->encoder && g->nv.nvEncGetLastErrorString?g->nv.nvEncGetLastErrorString(g->encoder):"";
    snprintf(error,cap,"%s: NVENC %d %s",where,status,why?why:"");
    return -1;
}

static int start_encoder(VoxGpu *g,uint32_t fps,uint32_t kbps,float quality,uint32_t preset_index,char *error,size_t cap) {
    if (nvenc_load_functions(&g->loader,NULL)<0) {snprintf(error,cap,"NVENC library unavailable");return -1;}
    g->nv.version=NV_ENCODE_API_FUNCTION_LIST_VER;
    if (nvcheck(g,g->loader->NvEncodeAPICreateInstance(&g->nv),error,cap,"NVENC API")<0) return -1;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open={.version=NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER};
    open.deviceType=NV_ENC_DEVICE_TYPE_OPENGL;
    open.apiVersion=NVENCAPI_VERSION;
    if (nvcheck(g,g->nv.nvEncOpenEncodeSessionEx(&open,&g->encoder),error,cap,"NVENC OpenGL session")<0) return -1;
    NV_ENC_PRESET_CONFIG preset={.version=NV_ENC_PRESET_CONFIG_VER};
    preset.presetCfg.version=NV_ENC_CONFIG_VER;
    const GUID presets[]={NV_ENC_PRESET_P1_GUID,NV_ENC_PRESET_P2_GUID,NV_ENC_PRESET_P3_GUID,
        NV_ENC_PRESET_P4_GUID,NV_ENC_PRESET_P5_GUID,NV_ENC_PRESET_P6_GUID,NV_ENC_PRESET_P7_GUID};
    GUID selected=presets[preset_index-1];
    if (nvcheck(g,g->nv.nvEncGetEncodePresetConfigEx(g->encoder,NV_ENC_CODEC_H264_GUID,
        selected,NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,&preset),error,cap,"NVENC preset")<0) return -1;
    g->config=preset.presetCfg;
    g->config.profileGUID=NV_ENC_H264_PROFILE_HIGH_GUID;
    g->config.gopLength=NVENC_INFINITE_GOPLENGTH;g->config.frameIntervalP=1;
    g->config.rcParams.rateControlMode=NV_ENC_PARAMS_RC_VBR;
    g->config.rcParams.averageBitRate=kbps*1000;g->config.rcParams.maxBitRate=kbps*1000;
    g->config.rcParams.vbvBufferSize=kbps*1000/fps;
    g->config.rcParams.vbvInitialDelay=g->config.rcParams.vbvBufferSize;
    g->config.rcParams.targetQuality=(uint8_t)quality;
    g->config.rcParams.targetQualityLSB=(uint8_t)((quality-(uint8_t)quality)*256);
    g->config.rcParams.enableLookahead=0;g->config.rcParams.enableAQ=0;g->config.rcParams.enableTemporalAQ=0;
    g->config.rcParams.zeroReorderDelay=1;g->config.rcParams.multiPass=NV_ENC_MULTI_PASS_DISABLED;
    NV_ENC_CONFIG_H264 *h=&g->config.encodeCodecConfig.h264Config;
    h->idrPeriod=NVENC_INFINITE_GOPLENGTH;h->repeatSPSPPS=1;h->outputAUD=0;
    h->h264VUIParameters.colourDescriptionPresentFlag=1;
    h->h264VUIParameters.videoSignalTypePresentFlag=1;
    h->h264VUIParameters.videoFormat=5;
    h->h264VUIParameters.colourPrimaries=1;h->h264VUIParameters.transferCharacteristics=13;
    h->h264VUIParameters.colourMatrix=6;h->h264VUIParameters.videoFullRangeFlag=0;
    g->init.version=NV_ENC_INITIALIZE_PARAMS_VER;g->init.encodeGUID=NV_ENC_CODEC_H264_GUID;
    g->init.presetGUID=selected;g->init.tuningInfo=NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    g->init.encodeWidth=g->init.maxEncodeWidth=g->init.darWidth=g->width;
    g->init.encodeHeight=g->init.maxEncodeHeight=g->init.darHeight=g->height;
    g->init.frameRateNum=fps;g->init.frameRateDen=1;g->init.enablePTD=1;
    g->init.encodeConfig=&g->config;
    if (nvcheck(g,g->nv.nvEncInitializeEncoder(g->encoder,&g->init),error,cap,"initialize NVENC")<0) return -1;
    for (int i=0;i<TARGETS;i++) {
        g->resource[i].texture=g->encode_texture[i];g->resource[i].target=GL_TEXTURE_2D;
        NV_ENC_REGISTER_RESOURCE resource={.version=NV_ENC_REGISTER_RESOURCE_VER};
        resource.resourceType=NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX;
        resource.width=g->width;resource.height=g->height;resource.pitch=g->width*4;
        resource.resourceToRegister=&g->resource[i];resource.bufferFormat=NV_ENC_BUFFER_FORMAT_ABGR;
        resource.bufferUsage=NV_ENC_INPUT_IMAGE;
        if (nvcheck(g,g->nv.nvEncRegisterResource(g->encoder,&resource),error,cap,"register capture texture")<0) return -1;
        g->registered[i]=resource.registeredResource;
    }
    NV_ENC_CREATE_BITSTREAM_BUFFER bitstream={.version=NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    if (nvcheck(g,g->nv.nvEncCreateBitstreamBuffer(g->encoder,&bitstream),error,cap,"NVENC bitstream")<0) return -1;
    g->bitstream=bitstream.bitstreamBuffer;g->kbps=kbps;g->quality=quality;
    return 0;
}

VoxGpu *voxl_gpu_new(const char *path,uint32_t width,uint32_t height,uint32_t format,
    const uint64_t *mods,size_t count,uint32_t fps,uint32_t kbps,float quality,uint32_t preset,char *error,size_t cap) {
    if (!count || count>1024 || width<2 || height<2 || width>16384 || height>16384 || !fps || !kbps || preset<1 || preset>7 || !(quality>=0 && quality<=51)) {
        snprintf(error,cap,"invalid GPU capture constraints");return NULL;
    }
    VoxGpu *g=calloc(1,sizeof(*g));if (!g) return NULL;
    g->fd=-1;g->display=EGL_NO_DISPLAY;g->context=EGL_NO_CONTEXT;g->width=width;g->height=height;
    g->fd=open(path,O_RDWR|O_CLOEXEC);
    if (g->fd<0) {snprintf(error,cap,"open render node: %s",strerror(errno));goto fail;}
    g->gbm=gbm_create_device(g->fd);
    if (!g->gbm) {snprintf(error,cap,"GBM device unavailable");goto fail;}
    g->display=eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR,g->gbm,NULL);
    if (g->display==EGL_NO_DISPLAY || !eglInitialize(g->display,NULL,NULL)) {snprintf(error,cap,"EGL initialize %#x",eglGetError());goto fail;}
    if (!eglBindAPI(EGL_OPENGL_API)) {snprintf(error,cap,"EGL OpenGL API unavailable");goto fail;}
    EGLConfig config;EGLint found;
    const EGLint attributes[]={EGL_RENDERABLE_TYPE,EGL_OPENGL_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_NONE};
    if (!eglChooseConfig(g->display,attributes,&config,1,&found)||!found) {snprintf(error,cap,"EGL configuration unavailable");goto fail;}
    const EGLint ctxattrs[]={EGL_CONTEXT_MAJOR_VERSION,3,EGL_CONTEXT_MINOR_VERSION,3,EGL_CONTEXT_OPENGL_PROFILE_MASK,EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,EGL_NONE};
    g->context=eglCreateContext(g->display,config,EGL_NO_CONTEXT,ctxattrs);
    if (g->context==EGL_NO_CONTEXT || !eglMakeCurrent(g->display,EGL_NO_SURFACE,EGL_NO_SURFACE,g->context)) {snprintf(error,cap,"EGL context %#x",eglGetError());goto fail;}
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target=(PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!image_target) {snprintf(error,cap,"EGL image texture import unavailable");goto fail;}
    for (int i=0;i<TARGETS;i++) {
        g->bo[i]=gbm_bo_create_with_modifiers2(g->gbm,width,height,format,mods,count,GBM_BO_USE_RENDERING);
        if (!g->bo[i]) {snprintf(error,cap,"GBM allocation for advertised modifiers: %s",strerror(errno));goto fail;}
        int planes=gbm_bo_get_plane_count(g->bo[i]);
        if (planes<1 || planes>4) {snprintf(error,cap,"invalid DMA-BUF plane count");goto fail;}
        uint64_t mod=gbm_bo_get_modifier(g->bo[i]);
        EGLAttrib attrib[64];int n=0;int fds[4]={-1,-1,-1,-1};
#define ATTR(k,v) do {attrib[n++]=(k);attrib[n++]=(EGLAttrib)(v);} while(0)
        ATTR(EGL_WIDTH,width);ATTR(EGL_HEIGHT,height);ATTR(EGL_LINUX_DRM_FOURCC_EXT,format);
        const int fdkeys[]={EGL_DMA_BUF_PLANE0_FD_EXT,EGL_DMA_BUF_PLANE1_FD_EXT,EGL_DMA_BUF_PLANE2_FD_EXT,EGL_DMA_BUF_PLANE3_FD_EXT};
        const int offkeys[]={EGL_DMA_BUF_PLANE0_OFFSET_EXT,EGL_DMA_BUF_PLANE1_OFFSET_EXT,EGL_DMA_BUF_PLANE2_OFFSET_EXT,EGL_DMA_BUF_PLANE3_OFFSET_EXT};
        const int pitchkeys[]={EGL_DMA_BUF_PLANE0_PITCH_EXT,EGL_DMA_BUF_PLANE1_PITCH_EXT,EGL_DMA_BUF_PLANE2_PITCH_EXT,EGL_DMA_BUF_PLANE3_PITCH_EXT};
        const int lowkeys[]={EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT};
        const int hikeys[]={EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT,EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT,EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT};
        for (int p=0;p<planes;p++) {
            fds[p]=gbm_bo_get_fd_for_plane(g->bo[i],p);
            ATTR(fdkeys[p],fds[p]);ATTR(offkeys[p],gbm_bo_get_offset(g->bo[i],p));ATTR(pitchkeys[p],gbm_bo_get_stride_for_plane(g->bo[i],p));
            if (mod!=DRM_FORMAT_MOD_INVALID) {ATTR(lowkeys[p],(uint32_t)mod);ATTR(hikeys[p],(uint32_t)(mod>>32));}
        }
        attrib[n]=EGL_NONE;
        g->image[i]=eglCreateImage(g->display,EGL_NO_CONTEXT,EGL_LINUX_DMA_BUF_EXT,NULL,attrib);
        for (int p=0;p<planes;p++) if (fds[p]>=0) close(fds[p]);
        if (g->image[i]==EGL_NO_IMAGE) {snprintf(error,cap,"EGL DMA-BUF import %#x",eglGetError());goto fail;}
        glGenTextures(1,&g->texture[i]);glBindTexture(GL_TEXTURE_2D,g->texture[i]);
        glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_NEAREST);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_NEAREST);
        image_target(GL_TEXTURE_2D,g->image[i]);
        glGenFramebuffers(1,&g->framebuffer[i]);glBindFramebuffer(GL_FRAMEBUFFER,g->framebuffer[i]);
        glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,g->texture[i],0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE) {snprintf(error,cap,"capture texture is not renderable");goto fail;}
        /* Imported GBM storage is not registerable by every NVENC driver. Keep
         * a normal RGBA8 texture registered and copy on-device before encoding. */
        glGenTextures(1,&g->encode_texture[i]);glBindTexture(GL_TEXTURE_2D,g->encode_texture[i]);
        glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_NEAREST);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_NEAREST);
        glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA8,width,height,0,GL_RGBA,GL_UNSIGNED_BYTE,NULL);
        glGenFramebuffers(1,&g->encode_framebuffer[i]);glBindFramebuffer(GL_FRAMEBUFFER,g->encode_framebuffer[i]);
        glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,g->encode_texture[i],0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE) {snprintf(error,cap,"encoder texture is not renderable");goto fail;}
    }
    if (start_encoder(g,fps,kbps,quality,preset,error,cap)<0) goto fail;
    return g;
fail:
    voxl_gpu_free(g);return NULL;
}

int voxl_gpu_export(VoxGpu *g,int i,VoxDmaBuf *d) {
    if (i<0||i>=TARGETS) return -1;
    memset(d,0,sizeof(*d));for(int p=0;p<4;p++)d->fd[p]=-1;
    struct gbm_bo *bo=g->bo[i];
    d->width=g->width;d->height=g->height;d->format=gbm_bo_get_format(bo);
    d->modifier=gbm_bo_get_modifier(bo);d->planes=gbm_bo_get_plane_count(bo);
    for(unsigned p=0;p<d->planes;p++) {
        d->fd[p]=gbm_bo_get_fd_for_plane(bo,p);d->stride[p]=gbm_bo_get_stride_for_plane(bo,p);d->offset[p]=gbm_bo_get_offset(bo,p);
        if(d->fd[p]<0){for(unsigned j=0;j<p;j++)close(d->fd[j]);return -1;}
    }
    return 0;
}

int voxl_gpu_encode(VoxGpu *g,int i,int key,uint32_t kbps,float quality,unsigned char *out,size_t cap,size_t *len,int *is_key,char *error,size_t errcap) {
    if(i<0||i>=TARGETS)return -1;
    glBindFramebuffer(GL_READ_FRAMEBUFFER,g->framebuffer[i]);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER,g->encode_framebuffer[i]);
    glBlitFramebuffer(0,0,g->width,g->height,0,0,g->width,g->height,GL_COLOR_BUFFER_BIT,GL_NEAREST);
    glFinish();
    if(kbps && (kbps!=g->kbps || quality!=g->quality)) {
        g->config.rcParams.averageBitRate=kbps*1000;g->config.rcParams.maxBitRate=kbps*1000;
        g->config.rcParams.vbvBufferSize=kbps*1000/g->init.frameRateNum;
        g->config.rcParams.targetQuality=(uint8_t)quality;
        g->config.rcParams.targetQualityLSB=(uint8_t)((quality-(uint8_t)quality)*256);
        NV_ENC_RECONFIGURE_PARAMS re={.version=NV_ENC_RECONFIGURE_PARAMS_VER};re.reInitEncodeParams=g->init;
        if(nvcheck(g,g->nv.nvEncReconfigureEncoder(g->encoder,&re),error,errcap,"reconfigure bitrate")<0)return -1;
        g->kbps=kbps;g->quality=quality;
    }
    NV_ENC_MAP_INPUT_RESOURCE map={.version=NV_ENC_MAP_INPUT_RESOURCE_VER};map.registeredResource=g->registered[i];
    if(nvcheck(g,g->nv.nvEncMapInputResource(g->encoder,&map),error,errcap,"map capture texture")<0)return -1;
    NV_ENC_PIC_PARAMS pic={.version=NV_ENC_PIC_PARAMS_VER};
    pic.inputBuffer=map.mappedResource;pic.bufferFmt=map.mappedBufferFmt;
    pic.inputWidth=g->width;pic.inputHeight=g->height;pic.outputBitstream=g->bitstream;
    pic.pictureStruct=NV_ENC_PIC_STRUCT_FRAME;pic.inputTimeStamp=++g->pts;
    if(key)pic.encodePicFlags=NV_ENC_PIC_FLAG_FORCEIDR|NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    int result=nvcheck(g,g->nv.nvEncEncodePicture(g->encoder,&pic),error,errcap,"encode picture");
    if(result==0) {
        NV_ENC_LOCK_BITSTREAM lock={.version=NV_ENC_LOCK_BITSTREAM_VER};lock.outputBitstream=g->bitstream;
        result=nvcheck(g,g->nv.nvEncLockBitstream(g->encoder,&lock),error,errcap,"receive encoded picture");
        if(result==0) {
            if(lock.bitstreamSizeInBytes>cap){snprintf(error,errcap,"encoded frame exceeds buffer");result=-1;}
            else {memcpy(out,lock.bitstreamBufferPtr,lock.bitstreamSizeInBytes);*len=lock.bitstreamSizeInBytes;*is_key=lock.pictureType==NV_ENC_PIC_TYPE_IDR;}
            g->nv.nvEncUnlockBitstream(g->encoder,g->bitstream);
        }
    }
    g->nv.nvEncUnmapInputResource(g->encoder,map.mappedResource);
    return result;
}
int voxl_gpu_readback(VoxGpu *g,int i,unsigned char *out,size_t len) {
    if(i<0||i>=TARGETS||len<(size_t)g->width*g->height*4)return -1;
    glBindFramebuffer(GL_FRAMEBUFFER,g->framebuffer[i]);glPixelStorei(GL_PACK_ALIGNMENT,1);
    glReadPixels(0,0,g->width,g->height,GL_BGRA,GL_UNSIGNED_BYTE,out);
    return glGetError()==GL_NO_ERROR?0:-1;
}
