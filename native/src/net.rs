use futures_util::{SinkExt, StreamExt};
use sdl2::event::{Event, EventSender};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

pub enum Incoming {
    Connected,
    Text(String),
    Binary(Vec<u8>),
    Disconnected(String),
    Closed(String),
}
pub struct Network {
    pub receive: mpsc::Receiver<Incoming>,
    send: mpsc::Sender<Message>,
    stop: Option<oneshot::Sender<()>>,
}
async fn forward(
    tx: &mpsc::Sender<Incoming>,
    wake: &EventSender,
    event_type: u32,
    message: Incoming,
) -> Result<(), String> {
    tx.send(message).await.map_err(|_| "viewer stopped")?;
    // No polling loop: a network arrival wakes the SDL event wait immediately.
    wake.push_event(Event::User {
        timestamp: 0,
        window_id: 0,
        type_: event_type,
        code: 0,
        data1: std::ptr::null_mut(),
        data2: std::ptr::null_mut(),
    })
}
impl Network {
    pub fn connect(address: &str, wake: EventSender, event_type: u32) -> Result<Self, String> {
        let mut url = url::Url::parse(address).map_err(|e| e.to_string())?;
        let scheme = match url.scheme() {
            "http" | "ws" => "ws",
            "https" | "wss" => "wss",
            _ => return Err("use http(s):// or ws(s)://".into()),
        };
        url.set_scheme(scheme).map_err(|_| "invalid scheme")?;
        url.set_path("/ws");
        url.set_fragment(None);
        if let Ok(token) = std::env::var("VOXL_TOKEN") {
            let pairs: Vec<_> = url
                .query_pairs()
                .filter(|(key, _)| key != "token")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            url.set_query(None);
            url.query_pairs_mut()
                .extend_pairs(pairs)
                .append_pair("token", &token);
        }
        let (tx, receive) = mpsc::channel(8);
        let (send, mut commands) = mpsc::channel::<Message>(1024);
        let reply = send.clone();
        let (stop, mut stopped) = oneshot::channel();
        std::thread::Builder::new().name("viewer-network".into()).spawn(move|| {
            let result:Result<(),String>=(||{
                let rt=tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e|e.to_string())?;
                rt.block_on(async {
                    let mut retry=1;
                    loop {
                        // Never replay stale input into a reconnected desktop.
                        while commands.try_recv().is_ok() {}
                        let mut config=WebSocketConfig::default();
                        config.max_message_size=Some(64*1024*1024);
                        config.max_frame_size=Some(64*1024*1024);
                        let connect=tokio_tungstenite::connect_async_with_config(url.as_str(),Some(config),true);
                        let attempt=tokio::select! {
                            _=&mut stopped=>return Ok(()),
                            r=tokio::time::timeout(Duration::from_secs(10),connect)=>r,
                        };
                        let reason=match attempt {
                            Ok(Ok((ws,_)))=>{
                                retry=1;
                                forward(&tx,&wake,event_type,Incoming::Connected).await?;
                                let (mut writer,mut reader)=ws.split();
                                let write=async {
                                    writer.send(Message::Text("v,1".into())).await.map_err(|e|e.to_string())?;
                                    while let Some(message)=commands.recv().await {
                                        writer.send(message).await.map_err(|e|e.to_string())?;
                                    }
                                    Ok::<(),String>(())
                                };
                                let read=async {
                                    while let Some(message)=reader.next().await {
                                        match message.map_err(|e|e.to_string())? {
                                            Message::Text(text)=>{
                                                if let Ok(m)=serde_json::from_str::<serde_json::Value>(&text) {
                                                    if m["t"]=="ping" {
                                                        reply.send(Message::Text(format!("po,{}",m["s"]).into())).await.map_err(|e|e.to_string())?;
                                                        continue;
                                                    }
                                                }
                                                forward(&tx,&wake,event_type,Incoming::Text(text.to_string())).await?;
                                            }
                                            Message::Binary(bytes)=>forward(&tx,&wake,event_type,Incoming::Binary(bytes.to_vec())).await?,
                                            Message::Ping(bytes)=>reply.send(Message::Pong(bytes)).await.map_err(|e|e.to_string())?,
                                            Message::Close(_)=>break,
                                            _=>{}
                                        }
                                    }
                                    Ok::<(),String>(())
                                };
                                let result=tokio::select!{_= &mut stopped=>return Ok(()),r=write=>r,r=read=>r};
                                result.err().unwrap_or_else(||"server closed the connection".into())
                            }
                            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
                                if matches!(response.status().as_u16(),401|403|404)=>return Err(format!("connection refused: HTTP {}",response.status())),
                            Ok(Err(error))=>error.to_string(),
                            Err(_)=>"connection timed out".into(),
                        };
                        forward(&tx,&wake,event_type,Incoming::Disconnected(reason)).await?;
                        tokio::select!{_=&mut stopped=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(retry))=>{}}
                        retry=(retry*2).min(8);
                    }
                })
            })();
            if let Err(error)=result {
                let _=tx.blocking_send(Incoming::Closed(error));
                let _=wake.push_event(Event::User{timestamp:0,window_id:0,type_:event_type,code:0,data1:std::ptr::null_mut(),data2:std::ptr::null_mut()});
            }
        }).map_err(|e|e.to_string())?;
        Ok(Self {
            receive,
            send,
            stop: Some(stop),
        })
    }
    pub fn command(&self, s: impl Into<String>) -> Result<(), String> {
        self.send
            .try_send(Message::Text(s.into().into()))
            .map_err(|e| format!("input queue: {e}"))
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
