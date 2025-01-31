// Copyright (C) 2021 Fernando Jimenez Moreno <fjimenez@igalia.com>
// Copyright (C) 2021-2022 Philippe Normand <philn@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::common::{caps, Credentials, Error, IpcMessage, StreamMessage, StreamMessageData};

use glib::subclass::prelude::*;
use glib::{clone, ToValue};
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_trace};
use gst_app::prelude::BaseTransformExt;
use ipc_channel::ipc::{IpcOneShotServer, IpcReceiver};
use once_cell::sync::{Lazy, OnceCell};
use signal_child::Signalable;
use std::path::Path;
use std::process::Child;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use url::Url;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "opentoksrc-remote",
        gst::DebugColorFlags::empty(),
        Some("OpenTok Source Remote"),
    )
});

/// Type of stream this source produces.
#[derive(Debug, PartialEq)]
enum Stream {
    Audio(String),
    Video(String),
}

pub struct OpenTokSrcRemote {
    /// Child process handler
    child_process: Arc<Mutex<Option<Child>>>,
    /// OpenTok session credentials (API key, session ID and token).
    credentials: Arc<Mutex<Credentials>>,
    /// OpenTok Stream identifier.
    /// We will be connecting to this stream only.
    stream_id: OnceCell<String>,
    /// Pad template for the video stream.
    video_src_pad_template: gst::PadTemplate,
    /// Pad template for the audio stream.
    audio_src_pad_template: gst::PadTemplate,
    /// Boolean flag to indicate whether the auxiliary threads should
    /// be running or not.
    aux_threads_running: Arc<AtomicBool>,
}

impl OpenTokSrcRemote {
    fn location(&self) -> Option<String> {
        self.credentials
            .lock()
            .unwrap()
            .session_id()
            .map(|id| format!("opentok://{}", id))
    }

    fn set_location(&self, location: &str) -> Result<(), glib::BoolError> {
        gst_debug!(CAT, "Setting location to {}", location);
        let url = Url::parse(location).map_err(|err| {
            glib::BoolError::new(
                format!("Malformed url {:?}", err),
                file!(),
                "set_location",
                line!(),
            )
        })?;
        let credentials: Credentials = url.into();
        gst_debug!(CAT, "Credentials {:?}", credentials);
        if let Some(ref stream_id) = credentials.stream_id() {
            if !stream_id.is_empty() {
                self.set_stream_id(stream_id.to_string())?;
            }
        }

        *self.credentials.lock().unwrap() = credentials;
        Ok(())
    }

    fn set_stream_id(&self, id: String) -> Result<(), glib::BoolError> {
        gst_debug!(CAT, "Setting stream ID to {}", id);
        self.stream_id.set(id).map_err(|_| {
            glib::BoolError::new(
                "Stream ID can only be set once",
                file!(),
                "set_stream_id",
                line!(),
            )
        })
    }

    fn launch_child_process(
        &self,
        ipc_server_name: &str,
    ) -> Result<(), Error> {
        gst_debug!(CAT, "Spawning child process");
        let mut command = std::process::Command::new("gst-opentok-helper");

        command.arg("--direction")
                .arg("src")
                .arg("--ipc-server")
                .arg(ipc_server_name);

        let credentials = self.credentials.lock().unwrap();
        if credentials.api_key().is_some() {
            command
                .arg("--api-key")
                .arg(credentials.api_key().unwrap())
                .arg("--session-id")
                .arg(credentials.session_id().unwrap())
                .arg("--token")
                .arg(credentials.token().unwrap());
        } else {
            command
                .arg("--room-uri")
                .arg(credentials.room_uri().unwrap().as_str());

        }
        drop(credentials);

        if let Some(stream_id) = self.stream_id.get() {
            command.arg("--stream-id").arg(stream_id);
        }
        *self.child_process.lock().unwrap() = Some(
            command
                .spawn()
                .map_err(|_| Error::OpenTokRemoteLaunchFailed)?,
        );
        Ok(())
    }

    fn init_stream_pipeline(
        element: &gst::Element,
        stream_type: Stream,
        socket_path: String,
        pad_template: &gst::PadTemplate,
        pad_name: String,
    ) -> Result<(), Error> {
        gst_trace!(
            CAT,
            obj: element,
            "Initializing pipeline for {:?} with socket path {:?}",
            stream_type,
            socket_path
        );

        let path = Path::new(&socket_path);
        let socket_id = path.file_name().unwrap();

        let toplevel_bin = element.downcast_ref::<gst::Bin>().unwrap();
        let bin = gst::ElementFactory::make("bin", socket_id.to_str()).unwrap();
        let bin_ref = bin.downcast_ref::<gst::Bin>().unwrap();

        let shmsrc = gst::ElementFactory::make("shmsrc", Some(&format!("shmsrc_{}", pad_name)))
            .map_err(|_| Error::MissingElement("shmsrc"))?;
        shmsrc.set_property("is-live", true);
        shmsrc.set_property("do-timestamp", true);
        shmsrc.set_property("socket-path", &socket_path);

        let caps = match stream_type {
            Stream::Audio(ref caps) => gst::Caps::from_str(caps).unwrap(),
            Stream::Video(ref caps) => gst::Caps::from_str(caps).unwrap(),
        };

        let capsfilter =
            gst::ElementFactory::make("capsfilter", Some(&format!("capsfilter_{}", pad_name)))
                .map_err(|_| Error::MissingElement("capsfilter"))?;
        capsfilter.set_property("caps", &caps);

        bin_ref
            .add_many(&[&shmsrc, &capsfilter])
            .map_err(|_| Error::AddElement("shmsrc ! capsfilter"))?;
        shmsrc
            .link(&capsfilter)
            .map_err(|_| Error::LinkElements("shmsrc ! capsfilter"))?;

        let target_src_pad = capsfilter.static_pad("src").unwrap();
        let pad = gst::GhostPad::with_target(Some("src"), &target_src_pad).unwrap();

        if let Err(err) = bin.add_pad(&pad) {
            gst_error!(CAT, obj: element, "Failed to add pad {:?}", err);
        }

        toplevel_bin.add(&bin).unwrap();

        let bin_src_pad =
            gst::GhostPad::from_template_with_target(pad_template, Some(&pad_name), &pad).unwrap();
        toplevel_bin.add_pad(&bin_src_pad).unwrap();
        bin.sync_state_with_parent().unwrap();
        Ok(())
    }

    fn remove_stream(
        element: &gst::Element,
        stream_name: &str,
        socket_path: String,
    ) -> Result<(), Error> {
        gst_debug!(
            CAT,
            obj: element,
            "Removing {} with socket path {}",
            stream_name,
            socket_path
        );

        let path = Path::new(&socket_path);
        let socket_id = path.file_name().unwrap();

        let toplevel_bin = element.downcast_ref::<gst::Bin>().unwrap();
        let name = socket_id.to_str().unwrap();
        if let Some(ref bin) = toplevel_bin.by_name(name) {
            bin.set_locked_state(true);
            bin.send_event(gst::event::Eos::new());
            bin.set_state(gst::State::Null).unwrap();
            let _ = bin.state(None);
            toplevel_bin.remove(bin).unwrap();

            let pad = element.static_pad(stream_name).unwrap();
            toplevel_bin.remove_pad(&pad).unwrap();
            gst_debug!(CAT, obj: element, "Removed pad {}", stream_name,);
        }

        Ok(())
    }

    fn update_caps(element: &gst::Element, caps_str: String, pad_name: String) {
        gst_debug!(CAT, "Updating video caps for {} to {}", pad_name, caps_str);
        let toplevel_bin = element.downcast_ref::<gst::Bin>().unwrap();
        if let Some(ref capsfilter) = toplevel_bin.by_name(&format!("capsfilter_{}", pad_name)) {
            let caps = gst::Caps::from_str(&caps_str).unwrap();
            capsfilter.set_property("caps", &caps);

            // Renegotiation triggered from capsfilter to basesrc does not
            // happen because the basesrc caps is ANY, so manually trigger
            // downstream re-negotiation instead.
            unsafe {
                let basetransform = capsfilter.unsafe_cast_ref::<gst_base::BaseTransform>();
                let _ = basetransform.update_src_caps(&caps);
            }
        }
    }

    fn critical_error(
        error: &str,
        element: &gst::Element,
        child_process: &Arc<Mutex<Option<Child>>>,
    ) {
        gst_error!(CAT, obj: element, "{}", error);
        if let Some(mut child_process) = child_process.lock().unwrap().take() {
            let _ = child_process.interrupt();
        }
        element
            .post_message(gst::message::Error::new(
                gst::CoreError::Failed,
                &format!("Child process error {}", error),
            ))
            .unwrap();
    }

    fn init(
        &self,
        element: &gst::Element,
    ) -> Result<(), Error> {
        // Spawn the child process and the auxiliary threads and hand over the
        // ipc server name.
        let (ipc_server, ipc_server_name): (IpcOneShotServer<IpcReceiver<IpcMessage>>, String) =
            IpcOneShotServer::new().map_err(|_| Error::OpenTokRemoteLaunchFailed)?;

        self.launch_child_process(&ipc_server_name)?;

        let child_process = self.child_process.clone();

        let (audio_thread_sender, audio_thread_receiver) = mpsc::channel();
        let (video_thread_sender, video_thread_receiver) = mpsc::channel();

        let audio_thread_sender = Arc::new(Mutex::new(audio_thread_sender));
        let video_thread_sender = Arc::new(Mutex::new(video_thread_sender));

        self.aux_threads_running.store(true, Ordering::Relaxed);

        let aux_threads_running = &self.aux_threads_running;

        // Control thread
        thread::spawn(clone!(
            @weak element,
            @weak child_process,
            @weak aux_threads_running,
        => move || {
            gst_debug!(CAT, obj: &element, "Control thread running");
            let (_, ipc_receiver) = ipc_server.accept().unwrap();
            gst_debug!(CAT, obj: &element, "Got IPC receiver");
            loop {
                if !aux_threads_running.load(Ordering::Relaxed) {
                    break;
                }
                match ipc_receiver.try_recv() {
                    Ok(message) => {
                        gst_debug!(CAT, obj: &element, "IPC message received: {:?}", message);
                        match message {
                            IpcMessage::Error(err) => {
                                OpenTokSrcRemote::critical_error(
                                    &err,
                                    &element,
                                    &child_process,
                                );
                                break;
                            },
                            IpcMessage::Stream(stream_message) => {
                                match stream_message {
                                    StreamMessage::Audio(message) => audio_thread_sender
                                        .lock()
                                        .unwrap()
                                        .send(message)
                                        .unwrap(),
                                    StreamMessage::Video(message) => video_thread_sender
                                        .lock()
                                        .unwrap()
                                        .send(message)
                                        .unwrap(),
                                }
                            },
                            _ => {},
                        }
                    },
                    Err(_) => std::thread::sleep(std::time::Duration::from_micros(10000)),
                }
            }
            gst_debug!(CAT, obj: &element, "Control thread exiting");
        }));

        // Audio thread
        let audio_pad_template = &self.audio_src_pad_template;
        thread::spawn(clone!(
            @weak element,
            @strong audio_pad_template,
            @weak child_process,
            @weak aux_threads_running,
        => move || {
            gst_debug!(CAT, obj: &element, "Audio thread running");
            loop {
                if !aux_threads_running.load(Ordering::Relaxed) {
                    break;
                }
                match audio_thread_receiver.try_recv() {
                    Ok(res) => match res {
                        StreamMessageData::ShmSocketPathAdded(socket_path, caps, pad_name) => {
                            gst_debug!(CAT, obj: &element, "Audio socket added: {}", &socket_path);
                            if let Err(err) = OpenTokSrcRemote::init_stream_pipeline(
                                &element,
                                Stream::Audio(caps),
                                socket_path,
                                &audio_pad_template,
                                pad_name,
                            ) {
                                OpenTokSrcRemote::critical_error(&err.to_string(), &element, &child_process);
                            }
                        },
                        StreamMessageData::ShmSocketPathRemoved(socket_path, ipc_sender) => {
                            gst_debug!(CAT, obj: &element, "Audio socket removed: {}", &socket_path);
                            match OpenTokSrcRemote::remove_stream(
                                &element,
                                "audio_stream",
                                socket_path,
                            ) {
                                Ok(()) => ipc_sender.send(()).unwrap(),
                                Err(err) => OpenTokSrcRemote::critical_error(&err.to_string(), &element, &child_process),
                            }
                        },
                        _ => {}
                    },
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_micros(10000));
                    }
                }
            }
            gst_debug!(CAT, obj: &element, "Audio thread exiting");
        }));

        // Video thread
        let video_pad_template = &self.video_src_pad_template;
        thread::spawn(clone!(
            @weak element,
            @strong video_pad_template,
            @weak aux_threads_running,
        => move || {
            gst_debug!(CAT, obj: &element, "Video thread running");
            loop {
                if !aux_threads_running.load(Ordering::Relaxed) {
                    break;
                }
                match video_thread_receiver.try_recv() {
                    Ok(res) => match res {
                        StreamMessageData::ShmSocketPathAdded(socket_path, caps, pad_name) => {
                            gst_debug!(CAT, obj: &element, "Video socket added: {}", &socket_path);
                            if let Err(err) = OpenTokSrcRemote::init_stream_pipeline(
                                &element,
                                Stream::Video(caps),
                                socket_path,
                                &video_pad_template,
                                pad_name,
                            ) {
                                OpenTokSrcRemote::critical_error(&err.to_string(), &element, &child_process)
                            }
                        },
                        StreamMessageData::ShmSocketPathRemoved(socket_path, ipc_sender) => {
                            gst_debug!(CAT, obj: &element, "Video socket removed: {}", &socket_path);
                            match OpenTokSrcRemote::remove_stream(
                                &element,
                                "video_stream",
                                socket_path,
                            ) {
                                Ok(()) => ipc_sender.send(()).unwrap(),
                                Err(err) => OpenTokSrcRemote::critical_error(&err.to_string(), &element, &child_process),
                            }
                        },
                        StreamMessageData::CapsChanged(caps, pad_name) => {
                            OpenTokSrcRemote::update_caps(&element, caps, pad_name);
                        }
                    },
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_micros(10000));
                    }
                }
            }
            gst_debug!(CAT, obj: &element, "Video thread exiting");
        }));

        Ok(())
    }

    fn maybe_init(&self, element: &gst::Element) -> Result<(), Error> {
        let credentials = self.credentials.lock().unwrap();
        if credentials.is_complete() {
            drop(credentials);
            return self.init(element);
        }
        Ok(())
    }

    fn teardown(&self) {
        self.aux_threads_running.store(false, Ordering::Relaxed);

        if let Some(mut child_process) = self.child_process.lock().unwrap().take() {
            let _ = child_process.interrupt();
            gst_debug!(CAT, "Interrupted child process");
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OpenTokSrcRemote {
    const NAME: &'static str = "OpenTokSrcRemote";
    type Type = super::OpenTokSrcRemote;
    type ParentType = gst::Bin;
    type Interfaces = (gst::URIHandler,);

    fn with_class(klass: &Self::Class) -> Self {
        let video_src_pad_template = klass.pad_template("video_stream_%u").unwrap();
        let audio_src_pad_template = klass.pad_template("audio_stream").unwrap();
        Self {
            child_process: Default::default(),
            credentials: Default::default(),
            stream_id: Default::default(),
            video_src_pad_template,
            audio_src_pad_template,
            aux_threads_running: Default::default(),
        }
    }
}

impl ObjectImpl for OpenTokSrcRemote {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "location",
                    "Location",
                    "OpenTok session location (i.e. opentok-remote://<session id>/key=<api key>&token=<token>)",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecBoolean::new(
                    "is-live",
                    "Is Live",
                    "Always!",
                    true,
                    glib::ParamFlags::READABLE,
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
        gst_trace!(CAT, obj: obj, "Setting property {:?}", pspec.name());
        match pspec.name() {
            "location" => {
                let location = value.get::<String>().expect("expected a string");
                if let Err(e) = self.set_location(&location) {
                    gst_error!(CAT, obj: obj, "Failed to set location: {:?}", e)
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => self.location().to_value(),
            "is-live" => true.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for OpenTokSrcRemote {}

impl ElementImpl for OpenTokSrcRemote {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "OpenTok Source Remote",
                "Source/Network",
                "Receives audio and video streams from an OpenTok session in a separate process",
                "Fernando Jiménez Moreno <ferjm@igalia.com>, Philippe Normand <philn@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let (video_caps, audio_caps) = caps();

            let video_src_pad_template = gst::PadTemplate::new(
                "video_stream_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &video_caps,
            )
            .unwrap();

            let audio_src_pad_template = gst::PadTemplate::new(
                "audio_stream",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &audio_caps,
            )
            .unwrap();

            vec![video_src_pad_template, audio_src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Changing state {:?}", transition);

        if transition == gst::StateChange::ReadyToNull {
            self.teardown();
        }
        if transition == gst::StateChange::NullToReady {
            gst_debug!(CAT, obj: element, "OpenTokSrcRemote initialization");
            if let Err(e) = self.maybe_init(element.upcast_ref::<gst::Element>()) {
                gst_error!(
                    CAT,
                    obj: element,
                    "Failed to initialize OpenTokSourceRemote: {:?}",
                    e
                )
            }
        }

        let mut success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            success = gst::StateChangeSuccess::NoPreroll;
        }

        Ok(success)
    }
}

impl BinImpl for OpenTokSrcRemote {}

impl URIHandlerImpl for OpenTokSrcRemote {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["opentok-remote"]
    }

    fn uri(&self, _: &Self::Type) -> Option<String> {
        self.location()
    }

    fn set_uri(&self, _: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        self.set_location(uri)
            .map_err(|e| glib::Error::new(gst::CoreError::Failed, &format!("{:?}", e)))
    }
}

impl Drop for OpenTokSrcRemote {
    fn drop(&mut self) {
        gst_debug!(CAT, "Dropping OpenTokSrcRemote");

        self.teardown();
    }
}
