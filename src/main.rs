use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use async_broadcast::Receiver;
use async_broadcast::Sender;
use bytes::Bytes;
use cast_sender::namespace::media::MusicTrackMediaMetadata;
use cast_sender::namespace::media::MusicTrackMediaMetadataBuilder;
use derive_builder::Builder;
use futures_util::stream::Map;
use http_body_util::{combinators::BoxBody, StreamBody};
use hyper::header::HeaderValue;
use hyper::{Request, Response, StatusCode};

use futures_util::StreamExt;
use futures_util::TryStreamExt;
use log::debug;
use log::error;
use log::info;
use serde::Serialize;

const SAMPLE_RATE: u64 = 48000;
const CHANNEL_COUNT: u64 = 2;

pub struct PipeWireSink {}

impl PipeWireSink {
    pub fn start(
        receiver_name: &str,
        packet_sender: Sender<Vec<i16>>,
        meta_sender: Sender<String>,
    ) -> anyhow::Result<()> {
        use libspa::param::audio;
        use pipewire as pw;
        pw::init();

        let mainloop = pw::main_loop::MainLoop::new(None)?;
        let context = pw::context::Context::with_properties(
            &mainloop,
            pw::properties::properties! {
                *pw::keys::APP_NAME => "rust-chromecast-sink",
            },
        )?;
        let core = context.connect(None)?;

        let stream: pw::stream::Stream = pw::stream::Stream::new(
            &core,
            "pw-cast",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CLASS => "Audio/Sink",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_NAME => "pw-cast",
                *pw::keys::NODE_DESCRIPTION => format!("Cast to “{receiver_name}”"),
                *pw::keys::AUDIO_CHANNELS => format!("{CHANNEL_COUNT}"),
            },
        )?;
        let chan_size = std::mem::size_of::<i16>();
        let _listener: pw::stream::StreamListener<f32> = stream
            .add_local_listener()
            .state_changed(move |_stream, _ref, old_state, new_state| {
                debug!("PipeWire state changed: {old_state:?} -> {new_state:?}");
            })
            .param_changed(move |_, _, c, pod| match (c, pod) {
                (17, Some(pod)) => {
                    if let Ok(o) = pod.as_object() {
                        if let Some(p) = o.find_prop(libspa::utils::Id(2)) {
                            let mut props = HashMap::new();
                            if let Ok(s) = p.value().as_struct() {
                                let mut fields = s.fields();
                                if let Some(n) = fields.next().and_then(|e| e.get_int().ok()) {
                                    for _ in 0..n {
                                        if let Some((k, v)) = fields
                                            .next()
                                            .and_then(|f| f.as_string().ok())
                                            .and_then(|key| {
                                                fields
                                                    .next()
                                                    .and_then(|f| f.as_string().ok())
                                                    .and_then(|value| Some((key, value)))
                                            })
                                        {
                                            props.insert(k, v);
                                        }
                                    }
                                }
                                if let Some(name) = props.get("media.name") {
                                    debug!("PipeWire source media.name changed: {}", name);
                                    let _err = meta_sender.broadcast_blocking(name.to_string());
                                }
                            }
                        }
                    }
                }
                _ => (),
            })
            .process(move |stream, _acc| match stream.dequeue_buffer() {
                Some(mut buffer) => {
                    let data = buffer.datas_mut();
                    if data.is_empty() {
                        return;
                    }
                    let pw_data = &mut data[0];
                    let stride = chan_size * 2;
                    let n_frames = (pw_data.chunk().size() / stride as u32) as usize;
                    let mut final_buffer: Vec<i16> = Vec::with_capacity(n_frames);
                    if let Some(slice) = pw_data.data() {
                        for n_frame in 0..n_frames {
                            for n_channel in 0..CHANNEL_COUNT {
                                let start = n_frame * stride + (n_channel as usize * chan_size);
                                let end = start + chan_size;
                                let channel = &mut slice[start..end];
                                final_buffer.push(i16::from_ne_bytes(channel.try_into().unwrap()));
                            }
                        }
                    }
                    if !final_buffer.is_empty() {
                        packet_sender
                            .broadcast_blocking(final_buffer)
                            .expect("broadcast");
                    }
                }
                None => (),
            })
            .register()?;

        let mut audio_info = audio::AudioInfoRaw::new();
        audio_info.set_format(audio::AudioFormat::S16LE);
        audio_info.set_rate(SAMPLE_RATE as u32);
        audio_info.set_channels(CHANNEL_COUNT as u32);
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(pw::spa::pod::Object {
                type_: libspa_sys::SPA_TYPE_OBJECT_Format,
                id: libspa_sys::SPA_PARAM_EnumFormat,
                properties: audio_info.into(),
            }),
        )
        .unwrap()
        .0
        .into_inner();
        let mut params = [libspa::pod::Pod::from_bytes(&values).unwrap()];
        stream.connect(
            libspa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;
        info!("PipeWire sink is ready");
        mainloop.run();
        unreachable!()
    }
}

#[derive(Clone)]
pub struct PipeWireHandler {
    stream_receiver: Receiver<Vec<i16>>,
}

impl PipeWireHandler {
    fn start(cast_device: &CastDevice) -> anyhow::Result<(Self, Receiver<String>)> {
        let (mut packet_sender, stream_receiver) = async_broadcast::broadcast(10);
        let (mut meta_sender, meta_receiver) = async_broadcast::broadcast(10);
        packet_sender.set_overflow(true); // Circular.
        meta_sender.set_overflow(true); // Circular.
        let name = cast_device.name.to_owned();
        tokio::task::spawn_blocking(move || {
            PipeWireSink::start(&name, packet_sender, meta_sender).expect("start PipeWire sink");
        });
        Ok((Self { stream_receiver }, meta_receiver))
    }

    fn encode(&self) -> Map<Receiver<Vec<i16>>, impl FnMut(Vec<i16>) -> anyhow::Result<Bytes>> {
        let mut mp3_encoder = mp3lame_encoder::Builder::new().expect("create LAME builder");
        mp3_encoder
            .set_num_channels(CHANNEL_COUNT as u8)
            .expect("set channels");
        mp3_encoder
            .set_sample_rate(SAMPLE_RATE as u32)
            .expect("set sample rate");
        mp3_encoder
            .set_brate(mp3lame_encoder::Bitrate::Kbps320)
            .expect("set brate");
        mp3_encoder
            .set_quality(mp3lame_encoder::Quality::Best)
            .expect("set quality");
        let mut mp3_encoder = mp3_encoder.build().expect("initialize LAME encoder");
        debug!("Starting mp3 (lame) encoding");
        self.stream_receiver.clone().map(move |data| {
            let input = mp3lame_encoder::InterleavedPcm(&data.as_slice());
            let mut mp3_out_buffer: Vec<u8> = Vec::new();
            mp3_out_buffer.reserve(mp3lame_encoder::max_required_buffer_size(data.len() / 2));
            let encoded_size = mp3_encoder
                .encode(input, mp3_out_buffer.spare_capacity_mut())
                .expect("lame encode");
            unsafe {
                mp3_out_buffer.set_len(mp3_out_buffer.len().wrapping_add(encoded_size));
            }
            anyhow::Ok(Bytes::from(mp3_out_buffer))
        })
    }
}

impl hyper::service::Service<Request<hyper::body::Incoming>> for PipeWireHandler {
    type Response = Response<BoxBody<Bytes, anyhow::Error>>;

    type Error = anyhow::Error;

    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn call(&self, req: Request<hyper::body::Incoming>) -> Self::Future {
        info!(
            "Serving new HTTP stream request for {}",
            req.headers()
                .get("user-agent")
                .unwrap_or(&HeaderValue::from_static(""))
                .to_str()
                .unwrap_or_default()
        );
        let watch_stream = self.encode();
        let stream_body = StreamBody::new(watch_stream.map_ok(hyper::body::Frame::data));
        let boxed_body: BoxBody<Bytes, anyhow::Error> = BoxBody::new(stream_body);
        Box::pin(async move {
            anyhow::Ok(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "audio/mpeg")
                    .header("Cache-Control", "no-cache")
                    .header("Transfer-Encoding", "chunked")
                    .body(boxed_body)
                    .unwrap(),
            )
        })
    }
}

struct Chromecast {
    media_controller: cast_sender::MediaController,
}

#[derive(Debug, Clone)]
pub struct CastDevice {
    pub ip_addr: IpAddr,
    pub model: String,
    pub name: String,
    pub status: Option<String>,
}

pub fn parse_cast_devices<'a>(
    records: impl IntoIterator<Item = &'a mdns::Record>,
) -> Vec<CastDevice> {
    let mut devices: HashMap<String, CastDevice> = HashMap::new();
    let mut txt_records: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut srv_to_a_mapping: HashMap<String, String> = HashMap::new();

    // First pass: organize records by type and extract information
    for record in records.into_iter() {
        match &record.kind {
            mdns::RecordKind::TXT(txt_values) => {
                if record.name.contains("._googlecast._tcp.local") {
                    let device_id = record.name.split("._googlecast").next().unwrap_or_default();
                    let mut txt_map = HashMap::new();

                    for txt in txt_values {
                        if let Some(index) = txt.find('=') {
                            let (key, value) = txt.split_at(index);
                            // Skip the '=' character in the value
                            txt_map.insert(key.to_string(), value[1..].to_string());
                        }
                    }

                    txt_records.insert(device_id.to_string(), txt_map);
                }
            }
            mdns::RecordKind::SRV { target, .. } => {
                if record.name.contains("._googlecast._tcp.local") {
                    let device_id = record.name.split("._googlecast").next().unwrap_or_default();
                    srv_to_a_mapping.insert(target.clone(), device_id.to_string());
                }
            }
            mdns::RecordKind::A(ip) => {
                let hostname = &record.name;
                // Find which device this IP belongs to using our SRV mapping
                if let Some(device_id) = srv_to_a_mapping.get(hostname) {
                    // Create a basic device with just the IP address for now
                    devices.insert(
                        device_id.clone(),
                        CastDevice {
                            ip_addr: IpAddr::V4(*ip),
                            model: String::new(),
                            name: String::new(),
                            status: None,
                        },
                    );
                }
            }
            _ => {} // Ignore other record types
        }
    }

    for (device_id, txt_map) in txt_records {
        if let Some(device) = devices.get_mut(&device_id) {
            if let Some(model) = txt_map.get("md") {
                device.model = model.clone();
            }
            if let Some(name) = txt_map.get("fn") {
                device.name = name.clone();
            }
            if let Some(status) = txt_map.get("rs") {
                if !status.is_empty() {
                    device.status = Some(status.clone());
                }
            }
        }
    }
    devices.into_values().collect()
}

#[derive(Serialize, Clone, Debug, Default, Builder)]
#[serde(rename_all = "camelCase")]
#[builder(setter(strip_option, into), default)]
struct CustomDataMetadata {
    pub metadata: cast_sender::namespace::media::MusicTrackMediaMetadata,
}

#[derive(Serialize, Clone, Debug, Default, Builder)]
#[serde(rename_all = "camelCase")]
#[builder(setter(strip_option, into), default)]
struct CustomDataDeviceName {
    pub device_name: String,
}

impl Chromecast {
    pub async fn discover() -> anyhow::Result<Vec<CastDevice>> {
        let stream =
            mdns::discover::all("_googlecast._tcp.local", Duration::from_secs(2))?.listen();
        futures_util::pin_mut!(stream);
        let mut devices = HashMap::new();
        'outer: while let Some(Ok(response)) = stream.next().await {
            for d in parse_cast_devices(response.records()) {
                match devices.entry(d.ip_addr) {
                    Entry::Vacant(entry) => {
                        entry.insert(d);
                    }
                    Entry::Occupied(_) => break 'outer,
                }
            }
        }
        Ok(devices.values().into_iter().map(|v| v.clone()).collect())
    }

    pub async fn new(cast_device: &CastDevice) -> anyhow::Result<Self> {
        let receiver = cast_sender::Receiver::new();
        receiver.connect(&cast_device.ip_addr.to_string()).await?;
        let app = receiver
            .launch_app(cast_sender::AppId::Custom("46C1A819".to_owned()))
            .await?;
        debug!("Launched Cast receiver app");
        let media_controller = cast_sender::MediaController::new(app.clone(), receiver.clone())?;
        Ok(Self { media_controller })
    }

    pub async fn start(&self, listen_sock: &SocketAddr) -> anyhow::Result<()> {
        let addr = format!("http://{listen_sock}");
        let media_info = cast_sender::namespace::media::MediaInformationBuilder::default()
            .content_id(&addr)
            .stream_type(cast_sender::namespace::media::StreamType::Live)
            .content_type("audio/*")
            // 46C1A819 hooks into LOAD events and updates the device name.
            // https://github.com/philippe44/chromecast-receiver/blob/master/receiver.html#L109-L110
            .custom_data(serde_json::to_value(CustomDataDeviceName {
                device_name: "pw-cast".to_owned(),
            })?)
            .build()
            .unwrap();
        let _ = self.media_controller.stop().await;
        self.media_controller.load(media_info).await?;
        debug!("Loaded media at {addr}");
        Ok(())
    }

    pub async fn metadata(&self, metadata: MusicTrackMediaMetadata) -> anyhow::Result<()> {
        self.media_controller
            // 46C1A819 hooks into PLAY events and updates the metadata according to customData.metadata.
            // https://github.com/philippe44/chromecast-receiver/blob/master/receiver.html#L98-L99
            .start_custom(CustomDataMetadata { metadata })
            .await
            .context("start()")
    }
}

use clap::Parser;

#[derive(Debug, Parser)]
struct Cli {
    /// Specifies the device
    #[clap(short, long)]
    device: Option<String>,

    /// Specifies the port (must be a valid u16)
    #[clap(short, long)]
    force_port: Option<u16>,

    /// Enable metadata (default: true)
    #[clap(short, long)]
    metadata: Option<bool>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Cli::parse();

    let (port, listener) = {
        let port = args.force_port.unwrap_or(0);
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
        (listener.local_addr()?.port(), listener)
    };

    let listen_sock = SocketAddr::new(local_ip_address::local_ip()?, port);
    info!("Listening on {listen_sock}");

    let devices = Chromecast::discover().await?;
    let cast_device = match args.device {
        Some(matcher) => {
            let matcher = matcher.to_ascii_lowercase();
            let length = devices.len();
            devices
                .iter()
                .find(|d| {
                    d.name.to_ascii_lowercase().contains(&matcher)
                        || d.model.to_ascii_lowercase().contains(&matcher)
                })
                .expect(&format!(
                    "none of the {length} Cast devices matches “{matcher}”"
                ))
        }
        None => devices.iter().next().expect("no Cast devices found"),
    };
    info!("Using Cast device: {cast_device:?}");

    let cast = Chromecast::new(&cast_device).await?;

    let (backend, mut meta_receiver) = PipeWireHandler::start(&cast_device)?;

    tokio::spawn(async move {
        if let Err(err) = cast.start(&listen_sock).await {
            error!("Could not start casting: {err}");
            return;
        }
        if !args.metadata.unwrap_or(true) {
            return;
        }
        while let Ok(media_name) = meta_receiver.recv().await {
            let _err = cast
                .metadata(
                    MusicTrackMediaMetadataBuilder::default()
                        .title(&media_name)
                        .images(vec![cast_sender::ImageBuilder::default()
                            .url(generate_cover_svg_base64(&media_name))
                            .build()
                            .unwrap()])
                        .build()
                        .unwrap(),
                )
                .await;
        }
    });

    loop {
        let (tcp, _) = listener.accept().await?;
        let io = hyper_util::rt::TokioIo::new(tcp);
        let backend = backend.clone();
        tokio::task::spawn(async move {
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, backend)
                .await
            {
                info!("HTTP serving error (disconnected?) {err}");
            }
        });
    }
}

pub fn generate_cover_svg_base64(text: &str) -> String {
    let size = 900;
    use svg::node::element::ForeignObject;
    use svg::node::element::Path;
    use svg::node::element::Rectangle;
    use svg::Document;
    let rect = Rectangle::new()
        .set("width", size)
        .set("height", size)
        .set("fill", "#931C1C");
    let foreign_object = ForeignObject::new()
        .set("x", 10) // Padding from edges
        .set("y", 10)
        .set("width", size - 20) // Ensure text doesn't touch edges
        .set("height", size - 20)
        .add(svg::node::Blob::new(format!(
            r#"<div xmlns="http://www.w3.org/1999/xhtml" 
                    style="width:100%; height:100%; display:flex; justify-content:center; align-items:center; text-align:center; color:white; font-size:64px; font-family:sans-serif; word-wrap:break-word;">
                {}
            </div>"#,
            svg::node::Text::new(text).to_string(),
        )));
    let document = Document::new()
        .set("width", size)
        .set("height", size)
        .add(rect)
        .add(Path::new().set("d", "M0 607L129 529L257 646L386 532L514 615L643 616L771 673L900 567L900 901L771 901L643 901L514 901L386 901L257 901L129 901L0 901Z").set("fill", "#f5730"))
        .add(Path::new().set("d", "M0 609L129 664L257 633L386 639L514 715L643 617L771 685L900 595L900 901L771 901L643 901L514 901L386 901L257 901L129 901L0 901Z").set("fill", "#da5b09"))
        .add(Path::new().set("d", "M0 664L129 689L257 725L386 666L514 725L643 685L771 682L900 690L900 901L771 901L643 901L514 901L386 901L257 901L129 901L0 901Z").set("fill", "#be4407"))
        .add(Path::new().set("d", "M0 812L129 811L257 729L386 779L514 769L643 742L771 733L900 756L900 901L771 901L643 901L514 901L386 901L257 901L129 901L0 901Z").set("fill", "#a32d04"))
        .add(Path::new().set("d", "M0 810L129 857L257 792L386 852L514 826L643 831L771 842L900 855L900 901L771 901L643 901L514 901L386 901L257 901L129 901L0 901Z").set("fill", "#871400"))
        .add(foreign_object);
    let svg_string = document.to_string();
    format!(
        "data:image/svg+xml;base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, svg_string)
    )
}
