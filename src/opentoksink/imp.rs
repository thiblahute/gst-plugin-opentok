// Copyright (C) 2021 Fernando Jimenez Moreno <fjimenez@igalia.com>
// Copyright (C) 2021-2022 Philippe Normand <philn@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::common::{
    caps, otc_format_from_gst_format, pipe_opentok_to_gst_log, Credentials, Error, init
};

use byte_slice_cast::*;
use glib::subclass::prelude::*;
use glib::{clone, ToValue};
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_trace, gst_warning};
use once_cell::sync::Lazy;
use opentok::audio_device::{AudioDevice, AudioSampleData};
use opentok::log::{self, LogLevel};
use opentok::publisher::{Publisher, PublisherCallbacks};
use opentok::session::{Session, SessionCallbacks};
use opentok::video_capturer::{VideoCapturer, VideoCapturerCallbacks, VideoCapturerSettings};
use opentok::video_frame::VideoFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "opentoksink",
        gst::DebugColorFlags::empty(),
        Some("OpenTok Sink"),
    )
});

/// Stream type enumeration.
#[derive(Clone, Debug, PartialEq)]
enum StreamType {
    Audio,
    Video,
    Unknown__,
}

/// Element sink pad name to StreamType conversion.
impl From<&str> for StreamType {
    fn from(stream_type: &str) -> StreamType {
        match stream_type {
            "video_sink" => StreamType::Video,
            "audio_sink" => StreamType::Audio,
            _ => StreamType::Unknown__,
        }
    }
}

impl From<StreamType> for &'static str {
    fn from(stream_type: StreamType) -> &'static str {
        match stream_type {
            StreamType::Video => "video_sink",
            StreamType::Audio => "audio_sink",
            _ => "",
        }
    }
}

struct SignalEmitter {
    element: glib::object::WeakRef<gst::Element>,
}

impl SignalEmitter {
    pub fn emit_published_stream(&self, stream_id: &str, url: &str) {
        if let Some(element) = self.element.upgrade() {
            element.emit_by_name::<()>("published-stream", &[&stream_id, &url]);
        }
    }
}

#[derive(Default)]
pub struct OpenTokSink {
    /// OpenTok session credentials (API key, session ID and token).
    credentials: Arc<Mutex<Credentials>>,
    /// OpenTok Session instance.
    /// Every Vonage Video API video chat occurs within a session.
    /// You can think of a session as a “room” where clients can interact
    /// with one another in real-time.
    session: Arc<Mutex<Option<Session>>>,
    /// Boolean flag indicating whether we are connected to a session or not.
    /// No audio or video can be published until the session is connected.
    session_connected: Arc<AtomicBool>,
    /// OpenTok Publisher instance.
    publisher: Arc<Mutex<Option<Publisher>>>,
    /// Audio sink, if any.
    audio_sink: Arc<Mutex<Option<gst::Element>>>,
    /// Video sink, if any.
    video_sink: Arc<Mutex<Option<gst::Element>>>,
    /// Video capturer.
    video_capturer: Arc<Mutex<Option<VideoCapturer>>>,
    /// Published stream unique identifier.
    published_stream_id: Arc<Mutex<Option<String>>>,
    /// Takes care of signaling when the stream is published.
    signal_emitter: Arc<Mutex<Option<SignalEmitter>>>,
    video_caps: Arc<Mutex<Option<gst::Caps>>>,
}

impl OpenTokSink {
    fn location(&self) -> Option<String> {
        self.credentials
            .lock()
            .unwrap()
            .session_id()
            .map(|id| format!("opentok://{}", id))
    }

    fn set_location(&self, location: &str) -> Result<(), glib::BoolError> {
        gst_debug!(CAT, "Setting location to {}", location);
        let url = match Url::parse(location) {
            Ok(url) => url,
            Err(err) => {
                return Err(glib::BoolError::new(
                    format!("Malformed url {:?}", err),
                    file!(),
                    "set_location",
                    line!(),
                ))
            }
        };
        let credentials: Credentials = url.into();
        gst_debug!(CAT, "Credentials {:?}", credentials);
        *self.credentials.lock().unwrap() = credentials;
        Ok(())
    }

    fn init_session(
        &self,
        element: &gst::Element,
        api_key: &str,
        session_id: &str,
        token: &str,
    ) -> Result<(), Error> {
        gst_debug!(CAT, "Init session");
        if self.session.lock().unwrap().is_some() {
            gst_debug!(
                CAT,
                "OpenTok is not ready yet or session already initialized"
            );
            return Ok(());
        }

        let session_connected = &self.session_connected;
        let publisher = &self.publisher;
        let session_callbacks = SessionCallbacks::builder()
            .on_connected(clone!(
                @weak session_connected,
                @strong publisher,
            => move |session| {
                gst_debug!(CAT, "Session connected");
                session_connected.store(true, Ordering::Relaxed);
                if let Some(ref publisher) = *publisher.lock().unwrap() {
                    gst_debug!(CAT, "Publishing on session");
                    if let Err(err) = session.publish(publisher) {
                        gst_error!(CAT, "Session publish error {}", err);
                    }
                }
            }))
            .on_error(clone!(
                @weak element
            => move |_, error, _| {
                gst::element_error!(&element, gst::ResourceError::Read, [error]);
            }))
            .build();
        match Session::new(api_key, session_id, session_callbacks) {
            Ok(session) => {
                let result = session
                    .connect(token)
                    .map_err(|e| Error::Init(format!("Connection error {:?}", e)));
                *self.session.lock().unwrap() = Some(session);
                result
            }
            Err(err) => Err(Error::Init(format!(
                "Failed to create OpenTok session `{:?}",
                err
            ))),
        }
    }

    fn maybe_init_session(&self, element: &gst::Element) -> Result<(), Error> {
        gst_debug!(CAT, "Maybe init session");
        let credentials = self.credentials.lock().unwrap().clone();
        if let Some(ref api_key) = credentials.api_key() {
            if let Some(ref session_id) = credentials.session_id() {
                if let Some(ref token) = credentials.token() {
                    return self.init_session(element, api_key, session_id, token);
                }
            }
        }
        gst_debug!(CAT, "Not ready to init session yet");
        Ok(())
    }

    fn teardown(&self) {
        gst_debug!(CAT, "Teardown");
        if let Some(publisher) = self.publisher.lock().unwrap().as_ref() {
            gst_debug!(CAT, "Unpublishing");
            if let Err(e) = publisher.unpublish() {
                gst_error!(CAT, "Unpublish error {}", e);
            }
        }
        if self.session_connected.load(Ordering::Relaxed) {
            if let Some(session) = self.session.lock().unwrap().as_ref() {
                gst_debug!(CAT, "Disconnecting");
                if let Err(e) = session.disconnect() {
                    gst_error!(CAT, "Session disconnect error {}", e);
                }
            }
        }
    }

    fn sink_event(
        &self,
        pad: &gst::GhostPad,
        element: &super::OpenTokSink,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;
        gst_debug!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(e) => {
                let caps = e.caps_owned();
                // We could cache audio caps here too, but since the opentok SDK
                // doesn't seem to support more than one format it's not really
                // needed.
                let s = caps.structure(0).unwrap();
                if s.name().starts_with("video") {
                    *self.video_caps.lock().unwrap() = Some(caps);
                }
                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn setup_video_sink(sink: &gst::Element, video_capturer: &VideoCapturer) {
        gst_debug!(CAT, "Setting up video sink");

        let video_capturer_ = video_capturer.clone();
        let on_new_sample =
            move |appsink: &gst_app::AppSink| -> Result<gst::FlowSuccess, gst::FlowError> {
                let sample = appsink.pull_sample().unwrap();
                let buffer = sample.buffer_owned().unwrap();
                let caps = sample.caps().unwrap();
                let info = gst_video::VideoInfo::from_caps(caps).unwrap();
                let map = buffer.into_mapped_buffer_readable().unwrap();
                let frame = VideoFrame::new(
                    otc_format_from_gst_format(info.format()),
                    info.width() as i32,
                    info.height() as i32,
                    map.to_vec(),
                );
                gst_trace!(CAT, "Providing frame through video capturer");
                if let Err(error) = video_capturer_.provide_frame(0, &frame) {
                    gst_error!(CAT, "Cannot provide frame to video capturer: {}", error,);
                }
                Ok(gst::FlowSuccess::Ok)
            };
        let sink = sink.downcast_ref::<gst_app::AppSink>().unwrap();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(on_new_sample)
                .build(),
        );
        sink.sync_state_with_parent().unwrap();
    }

    fn setup_audio_sink(sink: &gst::Element) {
        let audio_device = AudioDevice::get_instance();
        let on_new_sample =
            move |appsink: &gst_app::AppSink| -> Result<gst::FlowSuccess, gst::FlowError> {
                let sample = appsink.pull_sample().unwrap();
                let buffer = sample.buffer_owned().unwrap();
                let map = buffer.into_mapped_buffer_readable().unwrap();
                gst_trace!(CAT, "Providing audio sample");
                audio_device
                    .lock()
                    .unwrap()
                    .push_audio_sample(AudioSampleData(map.as_slice_of::<i16>().unwrap().to_vec()));
                Ok(gst::FlowSuccess::Ok)
            };
        let sink = sink.downcast_ref::<gst_app::AppSink>().unwrap();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(on_new_sample)
                .build(),
        );
        sink.sync_state_with_parent().unwrap();
    }

    fn ensure_publisher(&self, element: &super::OpenTokSink) {
        if self.publisher.lock().unwrap().is_some() {
            return;
        }

        gst_debug!(CAT, obj: element, "Initializing publisher");

        // Even if we are only publishing audio for now, we need to add a video
        // capturer, in case a video pad is requested at some point.
        let credentials = &self.credentials;
        let published_stream_id = &self.published_stream_id;
        let video_sink = &self.video_sink;
        let video_capturer = &self.video_capturer;
        let video_capturer_callbacks = VideoCapturerCallbacks::builder()
            .start(clone!(
                @weak video_capturer,
                @weak video_sink,
                @weak element,
            => @default-return Ok(()), move |capturer| {
                gst_debug!(CAT, obj: &element, "Video capturer ready");
                if let Some(ref video_sink) = *video_sink.lock().unwrap() {
                    OpenTokSink::setup_video_sink(
                        video_sink,
                        capturer,
                    );
                }
                *video_capturer.lock().unwrap() = Some(capturer.clone());
                Ok(())
            }))
            .build();

        let mut settings = VideoCapturerSettings::default();

        if let Some(ref video_caps) = *self.video_caps.lock().unwrap() {
            match gst_video::VideoInfo::from_caps(video_caps) {
                Ok(info) => {
                    settings.width = info.width() as i32;
                    settings.height = info.height() as i32;
                    let fps = info.fps();
                    settings.fps = (fps.numer() / fps.denom()) as i32;
                    settings.format = otc_format_from_gst_format(info.format());
                }
                Err(_) => {
                    gst_warning!(CAT, obj: element, "Invalid video caps, using default capturer settings");
                }
            };
        } else {
            // Ideally we should be able to create the publisher without
            // capturer, but this is not yet supported in opentok-rs.
            gst_debug!(CAT, obj: element, "No video pad, using default capturer settings");
        }

        let video_capturer = VideoCapturer::new(settings, video_capturer_callbacks);
        gst_debug!(CAT, obj: element, "Video capturer created");

        let signal_emitter = &self.signal_emitter;
        let publisher_callbacks = PublisherCallbacks::builder()
            .on_stream_created(clone!(
                @weak element,
                @weak credentials,
                @weak published_stream_id,
                @weak signal_emitter,
            => move |_, stream| {
                *published_stream_id.lock().unwrap() = Some(stream.id());
                let credentials = credentials.lock().unwrap().clone();
                let url = format!("opentok://{}/{}?key={}&token={}",
                                  credentials.session_id().unwrap(),
                                  stream.id(),
                                  credentials.api_key().unwrap(),
                                  credentials.token().unwrap()
                );
                signal_emitter.lock().unwrap().as_ref().unwrap().emit_published_stream(&stream.id(), &url);
                gst_info!(CAT, obj: &element, "Publisher stream created {}. Url {}", stream.id(), url);
            }))
            .on_error(clone!(
                @weak element,
            => move |_, error, _| {
                gst_error!(CAT, obj: &element, "Publisher error {}", error,);
                element.post_error_message(
                    gst::error_msg!(
                        gst::LibraryError::Failed,
                        [
                            format!("Failed to start publishing stream: {:?}", error).as_ref()
                        ]
                    )
                );
            }))
            .build();
        let publisher = Publisher::new("opentoksink", Some(video_capturer), publisher_callbacks);

        if let Some(ref session) = *self.session.lock().unwrap() {
            if self.session_connected.load(Ordering::Relaxed) {
                if let Err(err) = session.publish(&publisher) {
                    gst_error!(CAT, "Session publish error {}", err);
                }
            }
        }

        *self.publisher.lock().unwrap() = Some(publisher);

        gst_debug!(CAT, "Publisher created");
    }

    fn publish_video(&self) -> Result<(), Error> {
        gst_debug!(CAT, "Publish video");

        if self.audio_sink.lock().unwrap().is_none() {
            gst_debug!(CAT, "Toggling audio off");
            if let Some(ref publisher) = *self.publisher.lock().unwrap() {
                if let Err(err) = publisher.toggle_audio(false) {
                    gst_warning!(CAT, "Error toggling audio off {}", err);
                }
            }
        }

        match *self.video_sink.lock().unwrap() {
            Some(ref video_sink) => {
                if let Some(ref video_capturer) = *self.video_capturer.lock().unwrap() {
                    OpenTokSink::setup_video_sink(video_sink, video_capturer);
                }
                gst_debug!(CAT, "Toggling video on");
                if let Some(ref publisher) = *self.publisher.lock().unwrap() {
                    if let Err(err) = publisher.toggle_video(true) {
                        gst_warning!(CAT, "Error toggling video on {}", err);
                    }
                }
            }
            None => {
                return Err(Error::InvalidState(
                    "Trying to publish video with no video sink {}",
                ))
            }
        }

        gst_debug!(CAT, "Ready to publish video");
        Ok(())
    }

    fn publish_audio(&self) -> Result<(), Error> {
        gst_debug!(CAT, "Publish audio");

        if self.video_sink.lock().unwrap().is_none() {
            if let Some(ref publisher) = *self.publisher.lock().unwrap() {
                if let Err(err) = publisher.toggle_video(false) {
                    gst_warning!(CAT, "Error toggling video off {}", err);
                }
            }
        }

        match *self.audio_sink.lock().unwrap() {
            Some(ref sink) => {
                OpenTokSink::setup_audio_sink(sink);
                if let Some(ref publisher) = *self.publisher.lock().unwrap() {
                    if let Err(err) = publisher.toggle_audio(true) {
                        gst_warning!(CAT, "Error toggling audio on {}", err);
                    }
                }
            }
            None => {
                return Err(Error::InvalidState(
                    "Trying to publish audio with no audio sink {}",
                ))
            }
        }

        Ok(())
    }

    fn create_video_sink(&self, element: &crate::OpenTokSink) -> Result<gst::Pad, Error> {
        let bin = element
            .upcast_ref::<gst::Element>()
            .clone()
            .downcast::<gst::Bin>()
            .unwrap();

        let appsink = gst::ElementFactory::make("appsink", Some("video-sink"))
            .map_err(|_| Error::MissingElement("appsink"))?;
        appsink.set_property("enable-last-sample", false);

        bin.add(&appsink)
            .map_err(|_| Error::AddElement("appsink"))?;

        let target_sink_pad = appsink.static_pad("sink").unwrap();
        *self.video_sink.lock().unwrap() = Some(appsink);
        self.publish_video()?;
        Ok(target_sink_pad)
    }

    fn create_audio_sink(&self, element: &crate::OpenTokSink) -> Result<gst::Pad, Error> {
        let bin = element
            .upcast_ref::<gst::Element>()
            .clone()
            .downcast::<gst::Bin>()
            .unwrap();

        let appsink = gst::ElementFactory::make("appsink", Some("audio-sink"))
            .map_err(|_| Error::MissingElement("appsink"))?;
        appsink.set_property("enable-last-sample", false);

        bin.add(&appsink)
            .map_err(|_| Error::AddElement("appsink"))?;

        let target_sink_pad = appsink.static_pad("sink").unwrap();
        *self.audio_sink.lock().unwrap() = Some(appsink);
        self.publish_audio()?;
        Ok(target_sink_pad)
    }

    fn setup_sink(
        &self,
        element: &crate::OpenTokSink,
        template: &gst::PadTemplate,
        stream_type: StreamType,
    ) -> Result<gst::Pad, Error> {
        let target_pad = match stream_type {
            StreamType::Video => self.create_video_sink(element),
            StreamType::Audio => self.create_audio_sink(element),
            _ => {
                unreachable!();
            }
        }?;

        let ghost_pad = gst::GhostPad::builder_with_template(template, Some(stream_type.into()))
            .event_function(|pad, parent, event| {
                OpenTokSink::catch_panic_pad_function(
                    parent,
                    || false,
                    |sink, element| sink.sink_event(pad, element, event),
                )
            })
            .build_with_target(&target_pad)
            .map_err(|_| Error::PadConstruction("sink pad", target_pad.name().to_string()))?;

        ghost_pad
            .set_active(true)
            .map_err(|_| Error::PadActivation("sink pad"))?;
        element.add_pad(&ghost_pad).unwrap();
        Ok(ghost_pad.upcast())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OpenTokSink {
    const NAME: &'static str = "OpenTokSink";
    type Type = super::OpenTokSink;
    type ParentType = gst::Bin;
    type Interfaces = (gst::URIHandler,);

    fn with_class(_: &Self::Class) -> Self {
        Self::default()
    }
}

impl ObjectImpl for OpenTokSink {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SINK);

        gst_debug!(CAT, obj: obj, "OpenTokSink initialization");

        log::enable_log(LogLevel::Error);

        // Make sure the audio device is ready before the session is initiated,
        // otherwise OpenTok will use the libwebrtc default audio device.
        let _ = AudioDevice::get_instance();

        pipe_opentok_to_gst_log(*CAT);

        let element = obj.upcast_ref::<gst::Element>();
        let element = element.downgrade();
        *self.signal_emitter.lock().unwrap() = Some(SignalEmitter { element });
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "api-key",
                    "ApiKey",
                    "OpenTok API key",
                    None,
                    glib::ParamFlags::WRITABLE,
                ),
                glib::ParamSpecString::new(
                    "location",
                    "Location",
                    "OpenTok session location (i.e. opentok://<session id>?key=<api key>&token=<token>)",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecString::new(
                    "session-id",
                    "SessionId",
                    "OpenTok session unique identifier",
                    None,
                    glib::ParamFlags::WRITABLE,
                ),
                glib::ParamSpecString::new(
                    "stream-id",
                    "StreamId",
                    "Unique identifier of the OpenTok stream this sink is publishing",
                    None,
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpecString::new(
                    "token",
                    "SessionToken",
                    "OpenTok session token",
                    None,
                    glib::ParamFlags::WRITABLE,
                ),
                glib::ParamSpecString::new(
                    "demo-room-uri",
                    "Room uri of the OpenTok demo",
                    "URI of the opentok demo room, eg. https://opentokdemo.tokbox.com/room/rust345",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        gst_debug!(CAT, "Set property {:?}", pspec.name());
        let log_if_err_fn = |res| {
            if let Err(err) = res {
                gst_error!(CAT, "Got error: {:?} while setting {}", err, pspec.name());
            }
        };

        match pspec.name() {
            "api-key" => {
                if let Ok(api_key) = value.get::<String>() {
                    log_if_err_fn(self.credentials.lock().unwrap().set_api_key(api_key));
                }
            }
            "location" => {
                let location = value.get::<String>().expect("expected a string");
                if let Err(e) = self.set_location(&location) {
                    gst_error!(CAT, obj: obj, "Failed to set location: {:?}", e)
                }
            }
            "session-id" => {
                if let Ok(session_id) = value.get::<String>() {
                    log_if_err_fn(self.credentials.lock().unwrap().set_session_id(session_id));
                }
            }
            "token" => {
                if let Ok(token) = value.get::<String>() {
                    log_if_err_fn(self.credentials.lock().unwrap().set_token(token));
                }
            }
            "demo-room-uri" => {
                log_if_err_fn(self.credentials.lock().unwrap().set_room_uri(value.get::<String>().expect("expected a string")));
            }
            _ => unimplemented!(),
        }
        let element = obj.clone().upcast::<gst::Element>();
        if let Err(e) = self.maybe_init_session(&element) {
            gst_error!(
                CAT,
                obj: obj,
                "Failed to initialize OpenTok session: {:?}",
                e
            )
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => self.location().to_value(),
            "demo-room-uri" => self.credentials.lock().unwrap().room_uri().map(|url| url.as_str()).to_value(),
            "stream-id" => self
                .published_stream_id
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "".into())
                .to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder(
                "published-stream",
                &[String::static_type().into(), String::static_type().into()],
                glib::types::Type::UNIT.into(),
            )
            .build()]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for OpenTokSink {}

impl ElementImpl for OpenTokSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        init();

        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "OpenTok Sink",
                "Sink/Network",
                "Publish audio and video streams to an OpenTok session",
                "Fernando Jiménez Moreno <ferjm@igalia.com>, Philippe Normand <philn@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let (video_caps, audio_caps) = caps();

            let video_sink_pad_template = gst::PadTemplate::new(
                "video_sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &video_caps,
            )
            .unwrap();

            let audio_sink_pad_template = gst::PadTemplate::new(
                "audio_sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &audio_caps,
            )
            .unwrap();

            vec![video_sink_pad_template, audio_sink_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        template: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let stream_type: StreamType = match template.name_template() {
            Some(name) => name.as_str().into(),
            None => return None,
        };

        gst_debug!(
            CAT,
            obj: element,
            "Setting up things to publish {:?}",
            stream_type
        );

        gst_debug!(CAT, "Requesting new pad {:?}", stream_type);

        if (stream_type == StreamType::Audio && self.audio_sink.lock().unwrap().is_some())
            | (stream_type == StreamType::Video && self.video_sink.lock().unwrap().is_some())
        {
            gst_error!(
                CAT,
                obj: element,
                "There is already an existing pad for a stream of type {:?}",
                stream_type
            );
            return None;
        }

        match self.setup_sink(element, template, stream_type) {
            Ok(pad) => Some(pad),
            Err(err) => {
                gst_error!(CAT, obj: element, "{}", err,);
                None
            }
        }
    }

    fn release_pad(&self, element: &Self::Type, pad: &gst::Pad) {
        gst_debug!(CAT, "Release pad {:?}", pad.name());

        let bin = match pad.name().as_str().into() {
            StreamType::Audio => self.audio_sink.lock().unwrap().take(),
            StreamType::Video => self.video_sink.lock().unwrap().take(),
            StreamType::Unknown__ => unreachable!(),
        };

        if let Some(ref bin) = bin {
            if element.by_name(&bin.name()).is_some() {
                bin.set_state(gst::State::Null).unwrap();
                let _ = bin.state(None);
                let _ = self.remove_element(element, bin);
            }
        }

        let _ = element.remove_pad(pad);
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "State changed {:?}", transition);
        if transition == gst::StateChange::NullToReady {
            async_std::task::block_on(
                self.credentials.lock().unwrap().load(Duration::from_secs(5))
            ).map_err(|error| {
                gst_error!(CAT, "Error changing state: {:?}", error);

                gst::StateChangeError
            })?;

            if let Err(e) = self.maybe_init_session(&element.clone().upcast()) {
                gst_error!(
                    CAT,
                    obj: element,
                    "Failed to initialize OpenTok session: {:?}",
                    e
                )
            }

        }
        if transition == gst::StateChange::PausedToPlaying {
            self.ensure_publisher(element);
        }
        let success = self.parent_change_state(element, transition)?;
        if transition == gst::StateChange::ReadyToNull {
            self.teardown();
        }
        Ok(success)
    }
}

impl BinImpl for OpenTokSink {}

impl URIHandlerImpl for OpenTokSink {
    const URI_TYPE: gst::URIType = gst::URIType::Sink;

    fn protocols() -> &'static [&'static str] {
        &["opentok"]
    }

    fn uri(&self, _: &Self::Type) -> Option<String> {
        self.location()
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        self.set_location(uri)
            .map_err(|e| glib::Error::new(gst::CoreError::Failed, &format!("{:?}", e)))?;
        self.maybe_init_session(element.upcast_ref::<gst::Element>())
            .map_err(|e| glib::Error::new(gst::CoreError::Failed, &format!("{:?}", e)))
    }
}
