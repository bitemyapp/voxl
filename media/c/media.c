/* Small ownership boundary around libavcodec. Frames remain reference counted;
 * decoded GPU frames are never downloaded unless explicitly requested. */
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/pixdesc.h>
#include <libavutil/error.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

typedef struct {
    AVCodecContext *codec;
    AVBufferRef *device;
    int hardware;
} VoxDecoder;

static enum AVPixelFormat get_cuda(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    (void)ctx;
    for (const enum AVPixelFormat *f = formats; *f != AV_PIX_FMT_NONE; ++f)
        if (*f == AV_PIX_FMT_CUDA) return *f;
    return AV_PIX_FMT_NONE;
}

static void error_text(char *out, size_t cap, int error, const char *where) {
    char text[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(error, text, sizeof(text));
    snprintf(out, cap, "%s: %s", where, text);
}

void voxl_decoder_free(VoxDecoder *d) {
    if (!d) return;
    avcodec_free_context(&d->codec);
    av_buffer_unref(&d->device);
    free(d);
}

VoxDecoder *voxl_decoder_new(int mode, char *error, size_t cap) {
    VoxDecoder *d = calloc(1, sizeof(*d));
    if (!d) return NULL;
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_H264);
    if (!codec) { snprintf(error, cap, "H.264 decoder unavailable"); goto fail; }
    d->codec = avcodec_alloc_context3(codec);
    if (!d->codec) goto fail;
    d->codec->thread_count = 1;
    d->codec->max_pixels = 64 * 1024 * 1024;
    d->codec->flags |= AV_CODEC_FLAG_LOW_DELAY;
    d->codec->pkt_timebase = (AVRational){1, 1000000};
    if (mode != 0) {
        int ret = av_hwdevice_ctx_create(&d->device, AV_HWDEVICE_TYPE_CUDA, NULL, NULL, 0);
        if (ret < 0 && mode == 2) { error_text(error, cap, ret, "CUDA decoder device"); goto fail; }
        if (ret >= 0) {
            d->codec->hw_device_ctx = av_buffer_ref(d->device);
            d->codec->get_format = get_cuda;
            d->hardware = 1;
        }
    }
    int ret = avcodec_open2(d->codec, codec, NULL);
    if (ret < 0) { error_text(error, cap, ret, "open H.264 decoder"); goto fail; }
    return d;
fail:
    voxl_decoder_free(d);
    return NULL;
}

int voxl_decoder_hardware(VoxDecoder *d) { return d->hardware; }
int voxl_decoder_send(VoxDecoder *d, const unsigned char *data, size_t len, int64_t pts) {
    if (len > 64 * 1024 * 1024) return AVERROR(EINVAL);
    AVPacket *p = av_packet_alloc();
    if (!p) return AVERROR(ENOMEM);
    int ret = av_new_packet(p, (int)len);
    if (ret >= 0) {
        memcpy(p->data, data, len);
        p->pts = pts;
        p->dts = pts;
        ret = avcodec_send_packet(d->codec, p);
    }
    av_packet_free(&p);
    return ret;
}
AVFrame *voxl_decoder_receive(VoxDecoder *d, int *result) {
    AVFrame *frame = av_frame_alloc();
    if (!frame) { *result = AVERROR(ENOMEM); return NULL; }
    *result = avcodec_receive_frame(d->codec, frame);
    if (*result < 0) { av_frame_free(&frame); return NULL; }
    return frame;
}
void voxl_frame_free(AVFrame *f) { av_frame_free(&f); }
int voxl_frame_width(const AVFrame *f) { return f->width; }
int voxl_frame_height(const AVFrame *f) { return f->height; }
int64_t voxl_frame_pts(const AVFrame *f) { return f->pts; }
int voxl_frame_color(const AVFrame *f) {
    int matrix=f->colorspace==AVCOL_SPC_BT709?1:f->colorspace==AVCOL_SPC_BT2020_NCL?2:0;
    int full=f->color_range==AVCOL_RANGE_JPEG || f->format==AV_PIX_FMT_YUVJ420P;
    return matrix | (full?256:0);
}
/* Stable wire-independent layout: 0 = planar I420, 1 = NV12, 2 = CUDA. */
int voxl_frame_format(const AVFrame *f) {
    if (f->format == AV_PIX_FMT_YUV420P || f->format == AV_PIX_FMT_YUVJ420P) return 0;
    if (f->format == AV_PIX_FMT_NV12) return 1;
    if (f->format == AV_PIX_FMT_CUDA) return 2;
    return -1;
}
const unsigned char *voxl_frame_plane(const AVFrame *f, int i, int *stride) {
    if (i < 0 || i > 2 || voxl_frame_format(f) == 2) return NULL;
    *stride = f->linesize[i];
    return f->data[i];
}
int voxl_frame_download(const AVFrame *f, AVFrame **out) {
    *out = av_frame_alloc();
    if (!*out) return AVERROR(ENOMEM);
    int ret = av_hwframe_transfer_data(*out, f, 0);
    if (ret < 0) { av_frame_free(out); return ret; }
    ret=av_frame_copy_props(*out,f);
    if (ret < 0) {av_frame_free(out);return ret;}
    (*out)->pts = f->pts;
    return 0;
}

/* CUDA -> OpenGL copy stays on-device. GL resources are registered once per
 * decoder/size, mapped only while CUDA owns them, and unmapped before drawing. */
#include <ffnvcodec/dynlink_loader.h>
#include <libavutil/hwcontext_cuda.h>

typedef struct {
    CudaFunctions *cuda;
    AVBufferRef *device;
    CUcontext context;
    CUstream stream;
    CUgraphicsResource resources[2];
    CUevent copied;
    AVFrame *pending;
} VoxCudaGl;

void voxl_cuda_gl_free(VoxCudaGl *p) {
    if (!p) return;
    if (p->cuda && p->context && p->cuda->cuCtxPushCurrent(p->context) == CUDA_SUCCESS) {
        if (p->pending) p->cuda->cuEventSynchronize(p->copied);
        for (int i=0;i<2;i++) if (p->resources[i]) p->cuda->cuGraphicsUnregisterResource(p->resources[i]);
        if (p->copied) p->cuda->cuEventDestroy(p->copied);
        CUcontext previous;
        p->cuda->cuCtxPopCurrent(&previous);
    }
    /* A lost context cannot execute the pending copy, but the AVFrame still
     * owns host references that must be released on the fallback path. */
    av_frame_free(&p->pending);
    av_buffer_unref(&p->device);
    cuda_free_functions(&p->cuda);
    free(p);
}

VoxCudaGl *voxl_cuda_gl_new(const AVFrame *frame, unsigned y, unsigned uv, char *error, size_t cap) {
    if (frame->format != AV_PIX_FMT_CUDA || !frame->hw_frames_ctx) {
        snprintf(error,cap,"not a CUDA frame");return NULL;
    }
    AVHWFramesContext *frames=(AVHWFramesContext *)frame->hw_frames_ctx->data;
    if (frames->sw_format != AV_PIX_FMT_NV12) {
        snprintf(error,cap,"CUDA presentation requires NV12");return NULL;
    }
    VoxCudaGl *p=calloc(1,sizeof(*p));
    if (!p) return NULL;
    if (cuda_load_functions(&p->cuda,NULL)<0) {snprintf(error,cap,"CUDA driver entry points unavailable");goto fail;}
    p->device=av_buffer_ref(frames->device_ref);
    AVCUDADeviceContext *ctx=frames->device_ctx->hwctx;
    p->context=ctx->cuda_ctx;p->stream=ctx->stream;
    CUresult result=p->cuda->cuCtxPushCurrent(p->context);
    if (result!=CUDA_SUCCESS) {snprintf(error,cap,"CUDA context push: %d",result);goto fail;}
    result=p->cuda->cuGraphicsGLRegisterImage(&p->resources[0],y,0x0DE1,CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD);
    if (result==CUDA_SUCCESS)
        result=p->cuda->cuGraphicsGLRegisterImage(&p->resources[1],uv,0x0DE1,CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD);
    if (result==CUDA_SUCCESS) result=p->cuda->cuEventCreate(&p->copied,CU_EVENT_DISABLE_TIMING);
    CUcontext previous;p->cuda->cuCtxPopCurrent(&previous);
    if (result!=CUDA_SUCCESS) {snprintf(error,cap,"CUDA/GL registration: %d",result);goto fail;}
    return p;
fail:
    voxl_cuda_gl_free(p);return NULL;
}

int voxl_cuda_gl_copy(VoxCudaGl *p,const AVFrame *frame) {
    if (frame->format!=AV_PIX_FMT_CUDA) return -1;
    CUresult ret=p->cuda->cuCtxPushCurrent(p->context);
    if (ret!=CUDA_SUCCESS) return ret;
    int mapped=0;
    /* Hold the source AVFrame until its asynchronous copy completes. Returning
     * from unmap orders GL reads, but does not permit recycling decoder storage. */
    if (p->pending) {
        ret=p->cuda->cuEventSynchronize(p->copied);
        if (ret!=CUDA_SUCCESS) goto end;
        av_frame_free(&p->pending);
    }
    p->pending=av_frame_clone(frame);
    if (!p->pending) {ret=(CUresult)AVERROR(ENOMEM);goto end;}
    ret=p->cuda->cuGraphicsMapResources(2,p->resources,p->stream);
    if (ret!=CUDA_SUCCESS) goto end;
    mapped=1;
    for (int i=0;i<2;i++) {
        CUarray array;
        ret=p->cuda->cuGraphicsSubResourceGetMappedArray(&array,p->resources[i],0,0);
        if (ret!=CUDA_SUCCESS) break;
        CUDA_MEMCPY2D copy={0};
        copy.srcMemoryType=CU_MEMORYTYPE_DEVICE;
        copy.srcDevice=(CUdeviceptr)frame->data[i];
        copy.srcPitch=frame->linesize[i];
        copy.dstMemoryType=CU_MEMORYTYPE_ARRAY;copy.dstArray=array;
        copy.WidthInBytes=frame->width;
        copy.Height=i==0?frame->height:(frame->height+1)/2;
        ret=p->cuda->cuMemcpy2DAsync(&copy,p->stream);
        if (ret!=CUDA_SUCCESS) break;
    }
    if (ret==CUDA_SUCCESS) ret=p->cuda->cuEventRecord(p->copied,p->stream);
end:
    if (mapped) {
        CUresult unmap=p->cuda->cuGraphicsUnmapResources(2,p->resources,p->stream);
        if (ret==CUDA_SUCCESS) ret=unmap;
    }
    if (ret!=CUDA_SUCCESS && p->pending) {
        p->cuda->cuStreamSynchronize(p->stream);
        av_frame_free(&p->pending);
    }
    /* Unmapping establishes CUDA -> GL ordering. Do not block the CPU with a
     * device synchronization; GL waits for the dependency when sampling. */
    CUcontext previous;p->cuda->cuCtxPopCurrent(&previous);
    return ret;
}
