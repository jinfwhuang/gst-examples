#![recursion_limit = "256"]

mod macos_workaround;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use rand::prelude::*;

use structopt::StructOpt;

use async_std::prelude::*;
use async_std::task;
use futures::channel::mpsc;
use futures::sink::{Sink, SinkExt};
use futures::stream::StreamExt;

use async_tungstenite::tungstenite;
use tungstenite::Error as WsError;
use tungstenite::Message as WsMessage;

use gst::gst_element_error;
use gst::prelude::*;

use serde_derive::{Deserialize, Serialize};

use anyhow::{anyhow, bail, Context};

const STUN_SERVER: &str = "stun://stun.l.google.com:19302";
const TURN_SERVER: &str = "turn://foo:bar@webrtc.nirbheek.in:3478";
const VIDEO_WIDTH: u32 = 1024;
const VIDEO_HEIGHT: u32 = 768;

// upgrade weak reference or return
#[macro_export]
macro_rules! upgrade_weak {
    ($x:ident, $r:expr) => {{
        match $x.upgrade() {
            Some(o) => o,
            None => return $r,
        }
    }};
    ($x:ident) => {
        upgrade_weak!($x, ())
    };
}

#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(short, long, default_value = "wss://webrtc.nirbheek.in:8443")]
    server: String,
    #[structopt(short, long)]
    room_id: u32,
}

// JSON messages we communicate with
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

// Strong reference to our application state
#[derive(Debug, Clone)]
struct App(Arc<AppInner>);

// Weak reference to our application state
#[derive(Debug, Clone)]
struct AppWeak(Weak<AppInner>);

// Actual application state
#[derive(Debug)]
struct AppInner {
    args: Args,
    pipeline: gst::Pipeline,
    video_tee: gst::Element,
    audio_tee: gst::Element,
    video_mixer: gst::Element,
    audio_mixer: gst::Element,
    send_msg_tx: Arc<Mutex<mpsc::UnboundedSender<WsMessage>>>,
    peers: Mutex<BTreeMap<u32, Peer>>,
}

// Strong reference to the state of one peer
#[derive(Debug, Clone)]
struct Peer(Arc<PeerInner>);

// Weak reference to the state of one peer
#[derive(Debug, Clone)]
struct PeerWeak(Weak<PeerInner>);

// Actual peer state
#[derive(Debug)]
struct PeerInner {
    peer_id: u32,
    bin: gst::Bin,
    webrtcbin: gst::Element,
    send_msg_tx: Arc<Mutex<mpsc::UnboundedSender<WsMessage>>>,
}

// To be able to access the App's fields directly
impl std::ops::Deref for App {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.0
    }
}

// To be able to access the Peers's fields directly
impl std::ops::Deref for Peer {
    type Target = PeerInner;

    fn deref(&self) -> &PeerInner {
        &self.0
    }
}

impl AppWeak {
    // Try upgrading a weak reference to a strong one
    fn upgrade(&self) -> Option<App> {
        self.0.upgrade().map(App)
    }
}

impl PeerWeak {
    // Try upgrading a weak reference to a strong one
    fn upgrade(&self) -> Option<Peer> {
        self.0.upgrade().map(Peer)
    }
}

impl App {
    // Downgrade the strong reference to a weak reference
    fn downgrade(&self) -> AppWeak {
        AppWeak(Arc::downgrade(&self.0))
    }

    fn new(
        args: Args,
        initial_peers: &[&str],
    ) -> Result<
        (
            Self,
            impl Stream<Item = gst::Message>,
            impl Stream<Item = WsMessage>,
        ),
        anyhow::Error,
    > {
        // Create the GStreamer pipeline
        let pipeline = gst::parse_launch(
            &format!(
                "videotestsrc is-live=true ! vp8enc deadline=1 ! rtpvp8pay pt=96 ! tee name=video-tee ! \
                 queue ! fakesink sync=true \
                 audiotestsrc wave=ticks is-live=true ! opusenc ! rtpopuspay pt=97 ! tee name=audio-tee ! \
                 queue ! fakesink sync=true \
                 audiotestsrc wave=silence is-live=true ! audio-mixer. \
                 audiomixer name=audio-mixer sink_0::mute=true ! audioconvert ! audioresample ! autoaudiosink \
                 videotestsrc pattern=black ! capsfilter caps=video/x-raw,width=1,height=1 ! video-mixer. \
                 compositor name=video-mixer background=black sink_0::alpha=0.0 ! capsfilter caps=video/x-raw,width={width},height={height} ! videoconvert ! autovideosink",
                width=VIDEO_WIDTH,
                height=VIDEO_HEIGHT,
        ))?;

        // Downcast from gst::Element to gst::Pipeline
        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("not a pipeline");

        // Get access to the tees and mixers by name
        let video_tee = pipeline
            .get_by_name("video-tee")
            .expect("can't find video-tee");
        let audio_tee = pipeline
            .get_by_name("audio-tee")
            .expect("can't find audio-tee");

        let video_mixer = pipeline
            .get_by_name("video-mixer")
            .expect("can't find video-mixer");
        let audio_mixer = pipeline
            .get_by_name("audio-mixer")
            .expect("can't find audio-mixer");

        let bus = pipeline.get_bus().unwrap();

        // Send our bus messages via a futures channel to be handled asynchronously
        let (send_gst_msg_tx, send_gst_msg_rx) = mpsc::unbounded::<gst::Message>();
        let send_gst_msg_tx = Mutex::new(send_gst_msg_tx);
        bus.set_sync_handler(move |_, msg| {
            let _ = send_gst_msg_tx.lock().unwrap().unbounded_send(msg.clone());
            gst::BusSyncReply::Drop
        });

        // Channel for outgoing WebSocket messages from other threads
        let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded::<WsMessage>();

        // Asynchronously set the pipeline to Playing
        pipeline.call_async(|pipeline| {
            pipeline
                .set_state(gst::State::Playing)
                .expect("Couldn't set pipeline to Playing");
        });

        let app = App(Arc::new(AppInner {
            args,
            pipeline,
            video_tee,
            audio_tee,
            video_mixer,
            audio_mixer,
            peers: Mutex::new(BTreeMap::new()),
            send_msg_tx: Arc::new(Mutex::new(send_ws_msg_tx)),
        }));

        for peer in initial_peers {
            app.add_peer(peer, true)?;
        }

        // Asynchronously set the pipeline to Playing
        app.pipeline.call_async(|pipeline| {
            // If this fails, post an error on the bus so we exit
            if pipeline.set_state(gst::State::Playing).is_err() {
                gst_element_error!(
                    pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to set pipeline to Playing")
                );
            }
        });

        Ok((app, send_gst_msg_rx, send_ws_msg_rx))
    }

    // Handle WebSocket messages, both our own as well as WebSocket protocol messages
    fn handle_websocket_message(&self, msg: &str) -> Result<(), anyhow::Error> {
        if msg.starts_with("ERROR") {
            bail!("Got error message: {}", msg);
        }

        if msg.starts_with("ROOM_PEER_MSG ") {
            // Parse message and pass to the peer if we know about it
            let mut split = msg["ROOM_PEER_MSG ".len()..].splitn(2, ' ');
            let peer_id = split
                .next()
                .and_then(|s| str::parse::<u32>(s).ok())
                .ok_or_else(|| anyhow!("Can't parse peer id"))?;

            let peers = self.peers.lock().unwrap();
            let peer = peers
                .get(&peer_id)
                .ok_or_else(|| anyhow!("Can't find peer {}", peer_id))?
                .clone();
            drop(peers);

            let msg = split
                .next()
                .ok_or_else(|| anyhow!("Can't parse peer message"))?;

            let json_msg: JsonMsg = serde_json::from_str(msg)?;

            match json_msg {
                JsonMsg::Sdp { type_, sdp } => peer.handle_sdp(&type_, &sdp),
                JsonMsg::Ice {
                    sdp_mline_index,
                    candidate,
                } => peer.handle_ice(sdp_mline_index, &candidate),
            }
        } else if msg.starts_with("ROOM_PEER_JOINED ") {
            // Parse message and add the new peer
            let mut split = msg["ROOM_PEER_JOINED ".len()..].splitn(2, ' ');
            let peer_id = split.next().ok_or_else(|| anyhow!("Can't parse peer id"))?;

            self.add_peer(peer_id, false)
        } else if msg.starts_with("ROOM_PEER_LEFT ") {
            // Parse message and add the new peer
            let mut split = msg["ROOM_PEER_LEFT ".len()..].splitn(2, ' ');
            let peer_id = split.next().ok_or_else(|| anyhow!("Can't parse peer id"))?;

            self.remove_peer(peer_id)
        } else {
            Ok(())
        }
    }

    // Handle GStreamer messages coming from the pipeline
    fn handle_pipeline_message(&self, message: &gst::Message) -> Result<(), anyhow::Error> {
        use gst::message::MessageView;

        match message.view() {
            MessageView::Error(err) => bail!(
                "Error from element {}: {} ({})",
                err.get_src()
                    .map(|s| String::from(s.get_path_string()))
                    .unwrap_or_else(|| String::from("None")),
                err.get_error(),
                err.get_debug().unwrap_or_else(|| String::from("None")),
            ),
            MessageView::Warning(warning) => {
                println!("Warning: \"{}\"", warning.get_debug().unwrap());
            }
            _ => (),
        }

        Ok(())
    }

    // Add this new peer and if requested, send the offer to it
    fn add_peer(&self, peer: &str, offer: bool) -> Result<(), anyhow::Error> {
        println!("Adding peer {}", peer);
        let peer_id = str::parse::<u32>(peer).with_context(|| format!("Can't parse peer id"))?;
        let mut peers = self.peers.lock().unwrap();
        if peers.contains_key(&peer_id) {
            bail!("Peer {} already called", peer_id);
        }

        let peer_bin = gst::parse_bin_from_description(
            "queue name=video-queue ! webrtcbin. \
             queue name=audio-queue ! webrtcbin. \
             webrtcbin name=webrtcbin",
            false,
        )?;

        // Get access to the webrtcbin by name
        let webrtcbin = peer_bin
            .get_by_name("webrtcbin")
            .expect("can't find webrtcbin");

        // Set some properties on webrtcbin
        webrtcbin.set_property_from_str("stun-server", STUN_SERVER);
        webrtcbin.set_property_from_str("turn-server", TURN_SERVER);
        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

        // Add ghost pads for connecting to the input
        let audio_queue = peer_bin
            .get_by_name("audio-queue")
            .expect("can't find audio-queue");
        let audio_sink_pad = gst::GhostPad::new(
            Some("audio_sink"),
            &audio_queue.get_static_pad("sink").unwrap(),
        )
        .unwrap();
        peer_bin.add_pad(&audio_sink_pad).unwrap();

        let video_queue = peer_bin
            .get_by_name("video-queue")
            .expect("can't find video-queue");
        let video_sink_pad = gst::GhostPad::new(
            Some("video_sink"),
            &video_queue.get_static_pad("sink").unwrap(),
        )
        .unwrap();
        peer_bin.add_pad(&video_sink_pad).unwrap();

        let peer = Peer(Arc::new(PeerInner {
            peer_id,
            bin: peer_bin,
            webrtcbin,
            send_msg_tx: self.send_msg_tx.clone(),
        }));

        // Insert the peer into our map
        peers.insert(peer_id, peer.clone());
        drop(peers);

        // Add to the whole pipeline
        self.pipeline.add(&peer.bin).unwrap();

        // If we should send the offer to the peer, do so from on-negotiation-needed
        if offer {
            // Connect to on-negotiation-needed to handle sending an Offer
            let peer_clone = peer.downgrade();
            peer.webrtcbin
                .connect("on-negotiation-needed", false, move |values| {
                    let _webrtc = values[0].get::<gst::Element>().unwrap();

                    let peer = upgrade_weak!(peer_clone, None);
                    if let Err(err) = peer.on_negotiation_needed() {
                        gst_element_error!(
                            peer.bin,
                            gst::LibraryError::Failed,
                            ("Failed to negotiate: {:?}", err)
                        );
                    }

                    None
                })
                .unwrap();
        }

        // Whenever there is a new ICE candidate, send it to the peer
        let peer_clone = peer.downgrade();
        peer.webrtcbin
            .connect("on-ice-candidate", false, move |values| {
                let _webrtc = values[0].get::<gst::Element>().expect("Invalid argument");
                let mlineindex = values[1].get_some::<u32>().expect("Invalid argument");
                let candidate = values[2]
                    .get::<String>()
                    .expect("Invalid argument")
                    .unwrap();

                let peer = upgrade_weak!(peer_clone, None);

                if let Err(err) = peer.on_ice_candidate(mlineindex, candidate) {
                    gst_element_error!(
                        peer.bin,
                        gst::LibraryError::Failed,
                        ("Failed to send ICE candidate: {:?}", err)
                    );
                }

                None
            })
            .unwrap();

        // Whenever there is a new stream incoming from the peer, handle it
        let peer_clone = peer.downgrade();
        peer.webrtcbin.connect_pad_added(move |_webrtc, pad| {
            let peer = upgrade_weak!(peer_clone);

            if let Err(err) = peer.on_incoming_stream(pad) {
                gst_element_error!(
                    peer.bin,
                    gst::LibraryError::Failed,
                    ("Failed to handle incoming stream: {:?}", err)
                );
            }
        });

        // Whenever a decoded stream comes available, handle it and connect it to the mixers
        let app_clone = self.downgrade();
        peer.bin.connect_pad_added(move |_bin, pad| {
            let app = upgrade_weak!(app_clone);

            if pad.get_name() == "audio_src" {
                let audiomixer_sink_pad = app.audio_mixer.get_request_pad("sink_%u").unwrap();
                pad.link(&audiomixer_sink_pad).unwrap();

                // Once it is unlinked again later when the peer is being removed,
                // also release the pad on the mixer
                audiomixer_sink_pad.connect_unlinked(move |pad, _peer| {
                    if let Some(audiomixer) = pad.get_parent() {
                        let audiomixer = audiomixer.downcast_ref::<gst::Element>().unwrap();
                        audiomixer.release_request_pad(pad);
                    }
                });
            } else if pad.get_name() == "video_src" {
                let videomixer_sink_pad = app.video_mixer.get_request_pad("sink_%u").unwrap();
                pad.link(&videomixer_sink_pad).unwrap();

                app.relayout_videomixer();

                // Once it is unlinked again later when the peer is being removed,
                // also release the pad on the mixer
                let app_clone = app.downgrade();
                videomixer_sink_pad.connect_unlinked(move |pad, _peer| {
                    let app = upgrade_weak!(app_clone);

                    if let Some(videomixer) = pad.get_parent() {
                        let videomixer = videomixer.downcast_ref::<gst::Element>().unwrap();
                        videomixer.release_request_pad(pad);
                    }

                    app.relayout_videomixer();
                });
            }
        });

        // Add pad probes to both tees for blocking them and
        // then unblock them once we reached the Playing state.
        //
        // Then link them and unblock, in case they got blocked
        // in the meantime.
        //
        // Otherwise it might happen that data is received before
        // the elements are ready and then an error happens.
        let audio_src_pad = self.audio_tee.get_request_pad("src_%u").unwrap();
        let audio_block = audio_src_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        audio_src_pad.link(&audio_sink_pad)?;

        let video_src_pad = self.video_tee.get_request_pad("src_%u").unwrap();
        let video_block = video_src_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        video_src_pad.link(&video_sink_pad)?;

        // Asynchronously set the peer bin to Playing
        peer.bin.call_async(move |bin| {
            // If this fails, post an error on the bus so we exit
            if bin.sync_state_with_parent().is_err() {
                gst_element_error!(
                    bin,
                    gst::LibraryError::Failed,
                    ("Failed to set peer bin to Playing")
                );
            }

            // And now unblock
            audio_src_pad.remove_probe(audio_block);
            video_src_pad.remove_probe(video_block);
        });

        Ok(())
    }

    // Remove this peer
    fn remove_peer(&self, peer: &str) -> Result<(), anyhow::Error> {
        println!("Removing peer {}", peer);
        let peer_id = str::parse::<u32>(peer).with_context(|| format!("Can't parse peer id"))?;
        let mut peers = self.peers.lock().unwrap();
        if let Some(peer) = peers.remove(&peer_id) {
            drop(peers);

            // Now asynchronously remove the peer from the pipeline
            let app_clone = self.downgrade();
            self.pipeline.call_async(move |_pipeline| {
                let app = upgrade_weak!(app_clone);

                // Block the tees shortly for removal
                let audio_tee_sinkpad = app.audio_tee.get_static_pad("sink").unwrap();
                let audio_block = audio_tee_sinkpad
                    .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                        gst::PadProbeReturn::Ok
                    })
                    .unwrap();

                let video_tee_sinkpad = app.video_tee.get_static_pad("sink").unwrap();
                let video_block = video_tee_sinkpad
                    .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                        gst::PadProbeReturn::Ok
                    })
                    .unwrap();

                // Release the tee pads and unblock
                let audio_sinkpad = peer.bin.get_static_pad("audio_sink").unwrap();
                let video_sinkpad = peer.bin.get_static_pad("video_sink").unwrap();

                if let Some(audio_tee_srcpad) = audio_sinkpad.get_peer() {
                    let _ = audio_tee_srcpad.unlink(&audio_sinkpad);
                    app.audio_tee.release_request_pad(&audio_tee_srcpad);
                }
                audio_tee_sinkpad.remove_probe(audio_block);

                if let Some(video_tee_srcpad) = video_sinkpad.get_peer() {
                    let _ = video_tee_srcpad.unlink(&video_sinkpad);
                    app.video_tee.release_request_pad(&video_tee_srcpad);
                }
                video_tee_sinkpad.remove_probe(video_block);

                // Then remove the peer bin gracefully from the pipeline
                let _ = app.pipeline.remove(&peer.bin);
                let _ = peer.bin.set_state(gst::State::Null);

                println!("Removed peer {}", peer.peer_id);
            });
        }

        Ok(())
    }

    fn relayout_videomixer(&self) {
        let mut pads = self.video_mixer.get_sink_pads();
        if pads.is_empty() {
            return;
        }

        // We ignore the first pad
        pads.remove(0);
        let npads = pads.len();

        let (width, height) = if npads <= 1 {
            (1, 1)
        } else if npads <= 4 {
            (2, 2)
        } else if npads <= 16 {
            (4, 4)
        } else {
            // FIXME: we don't support more than 16 streams for now
            (4, 4)
        };

        let mut x: i32 = 0;
        let mut y: i32 = 0;
        let w = VIDEO_WIDTH as i32 / width;
        let h = VIDEO_HEIGHT as i32 / height;

        for pad in pads {
            pad.set_property("xpos", &x).unwrap();
            pad.set_property("ypos", &y).unwrap();
            pad.set_property("width", &w).unwrap();
            pad.set_property("height", &h).unwrap();

            x += w;
            if x >= VIDEO_WIDTH as i32 {
                x = 0;
                y += h;
            }
        }
    }
}

// Make sure to shut down the pipeline when it goes out of scope
// to release any system resources
impl Drop for AppInner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Peer {
    // Downgrade the strong reference to a weak reference
    fn downgrade(&self) -> PeerWeak {
        PeerWeak(Arc::downgrade(&self.0))
    }

    // Whenever webrtcbin tells us that (re-)negotiation is needed, simply ask
    // for a new offer SDP from webrtcbin without any customization and then
    // asynchronously send it to the peer via the WebSocket connection
    fn on_negotiation_needed(&self) -> Result<(), anyhow::Error> {
        println!("starting negotiation with peer {}", self.peer_id);

        let peer_clone = self.downgrade();
        let promise = gst::Promise::new_with_change_func(move |reply| {
            let peer = upgrade_weak!(peer_clone);

            if let Err(err) = peer.on_offer_created(reply) {
                gst_element_error!(
                    peer.bin,
                    gst::LibraryError::Failed,
                    ("Failed to send SDP offer: {:?}", err)
                );
            }
        });

        self.webrtcbin
            .emit("create-offer", &[&None::<gst::Structure>, &promise])
            .unwrap();

        Ok(())
    }

    // Once webrtcbin has create the offer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_offer_created(
        &self,
        reply: Result<&gst::StructureRef, gst::PromiseError>,
    ) -> Result<(), anyhow::Error> {
        let reply = match reply {
            Ok(reply) => reply,
            Err(err) => {
                bail!("Offer creation future got no reponse: {:?}", err);
            }
        };

        let offer = reply
            .get_value("offer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument")
            .unwrap();
        self.webrtcbin
            .emit("set-local-description", &[&offer, &None::<gst::Promise>])
            .unwrap();

        println!(
            "sending SDP offer to peer: {}",
            offer.get_sdp().as_text().unwrap()
        );

        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "offer".to_string(),
            sdp: offer.get_sdp().as_text().unwrap(),
        })
        .unwrap();

        self.send_msg_tx
            .lock()
            .unwrap()
            .unbounded_send(WsMessage::Text(format!(
                "ROOM_PEER_MSG {} {}",
                self.peer_id, message
            )))
            .with_context(|| format!("Failed to send SDP offer"))?;

        Ok(())
    }

    // Once webrtcbin has create the answer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_answer_created(
        &self,
        reply: Result<&gst::StructureRef, gst::PromiseError>,
    ) -> Result<(), anyhow::Error> {
        let reply = match reply {
            Ok(reply) => reply,
            Err(err) => {
                bail!("Answer creation future got no reponse: {:?}", err);
            }
        };

        let answer = reply
            .get_value("answer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument")
            .unwrap();
        self.webrtcbin
            .emit("set-local-description", &[&answer, &None::<gst::Promise>])
            .unwrap();

        println!(
            "sending SDP answer to peer: {}",
            answer.get_sdp().as_text().unwrap()
        );

        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "answer".to_string(),
            sdp: answer.get_sdp().as_text().unwrap(),
        })
        .unwrap();

        self.send_msg_tx
            .lock()
            .unwrap()
            .unbounded_send(WsMessage::Text(format!(
                "ROOM_PEER_MSG {} {}",
                self.peer_id, message
            )))
            .with_context(|| format!("Failed to send SDP answer"))?;

        Ok(())
    }

    // Handle incoming SDP answers from the peer
    fn handle_sdp(&self, type_: &str, sdp: &str) -> Result<(), anyhow::Error> {
        if type_ == "answer" {
            print!("Received answer:\n{}\n", sdp);

            let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                .map_err(|_| anyhow!("Failed to parse SDP answer"))?;
            let answer =
                gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, ret);

            self.webrtcbin
                .emit("set-remote-description", &[&answer, &None::<gst::Promise>])
                .unwrap();

            Ok(())
        } else if type_ == "offer" {
            print!("Received offer:\n{}\n", sdp);

            let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                .map_err(|_| anyhow!("Failed to parse SDP offer"))?;

            // And then asynchronously start our pipeline and do the next steps. The
            // pipeline needs to be started before we can create an answer
            let peer_clone = self.downgrade();
            self.bin.call_async(move |_pipeline| {
                let peer = upgrade_weak!(peer_clone);

                let offer = gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Offer,
                    ret,
                );

                peer.0
                    .webrtcbin
                    .emit("set-remote-description", &[&offer, &None::<gst::Promise>])
                    .unwrap();

                let peer_clone = peer.downgrade();
                let promise = gst::Promise::new_with_change_func(move |reply| {
                    let peer = upgrade_weak!(peer_clone);

                    if let Err(err) = peer.on_answer_created(reply) {
                        gst_element_error!(
                            peer.bin,
                            gst::LibraryError::Failed,
                            ("Failed to send SDP answer: {:?}", err)
                        );
                    }
                });

                peer.0
                    .webrtcbin
                    .emit("create-answer", &[&None::<gst::Structure>, &promise])
                    .unwrap();
            });

            Ok(())
        } else {
            bail!("Sdp type is not \"answer\" but \"{}\"", type_)
        }
    }

    // Handle incoming ICE candidates from the peer by passing them to webrtcbin
    fn handle_ice(&self, sdp_mline_index: u32, candidate: &str) -> Result<(), anyhow::Error> {
        self.webrtcbin
            .emit("add-ice-candidate", &[&sdp_mline_index, &candidate])
            .unwrap();

        Ok(())
    }

    // Asynchronously send ICE candidates to the peer via the WebSocket connection as a JSON
    // message
    fn on_ice_candidate(&self, mlineindex: u32, candidate: String) -> Result<(), anyhow::Error> {
        let message = serde_json::to_string(&JsonMsg::Ice {
            candidate,
            sdp_mline_index: mlineindex,
        })
        .unwrap();

        self.send_msg_tx
            .lock()
            .unwrap()
            .unbounded_send(WsMessage::Text(format!(
                "ROOM_PEER_MSG {} {}",
                self.peer_id, message
            )))
            .with_context(|| format!("Failed to send ICE candidate"))?;

        Ok(())
    }

    // Whenever there's a new incoming, encoded stream from the peer create a new decodebin
    // and audio/video sink depending on the stream type
    fn on_incoming_stream(&self, pad: &gst::Pad) -> Result<(), anyhow::Error> {
        // Early return for the source pads we're adding ourselves
        if pad.get_direction() != gst::PadDirection::Src {
            return Ok(());
        }

        let caps = pad.get_current_caps().unwrap();
        let s = caps.get_structure(0).unwrap();
        let media_type = s
            .get::<&str>("media")
            .expect("Invalid type")
            .ok_or_else(|| anyhow!("no media type in caps {:?}", caps))?;

        let conv = if media_type == "video" {
            gst::parse_bin_from_description(
                &format!(
                    "decodebin name=dbin ! queue ! videoconvert ! videoscale ! capsfilter name=src caps=video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1",
                    width=VIDEO_WIDTH,
                    height=VIDEO_HEIGHT
                ),
                false,
            )?
        } else if media_type == "audio" {
            gst::parse_bin_from_description(
                "decodebin name=dbin ! queue ! audioconvert ! audioresample name=src",
                false,
            )?
        } else {
            println!("Unknown pad {:?}, ignoring", pad);
            return Ok(());
        };

        // Add a ghost pad on our conv bin that proxies the sink pad of the decodebin
        let dbin = conv.get_by_name("dbin").unwrap();
        let sinkpad =
            gst::GhostPad::new(Some("sink"), &dbin.get_static_pad("sink").unwrap()).unwrap();
        conv.add_pad(&sinkpad).unwrap();

        // And another one that proxies the source pad of the last element
        let src = conv.get_by_name("src").unwrap();
        let srcpad = gst::GhostPad::new(Some("src"), &src.get_static_pad("src").unwrap()).unwrap();
        conv.add_pad(&srcpad).unwrap();

        self.bin.add(&conv).unwrap();
        conv.sync_state_with_parent()
            .with_context(|| format!("can't start sink for stream {:?}", caps))?;

        pad.link(&sinkpad)
            .with_context(|| format!("can't link sink for stream {:?}", caps))?;

        // And then add a new ghost pad to the peer bin that proxies the source pad we added above
        if media_type == "video" {
            let srcpad = gst::GhostPad::new(Some("video_src"), &srcpad).unwrap();
            srcpad.set_active(true).unwrap();
            self.bin.add_pad(&srcpad).unwrap();
        } else if media_type == "audio" {
            let srcpad = gst::GhostPad::new(Some("audio_src"), &srcpad).unwrap();
            srcpad.set_active(true).unwrap();
            self.bin.add_pad(&srcpad).unwrap();
        }

        Ok(())
    }
}

// At least shut down the bin here if it didn't happen so far
impl Drop for PeerInner {
    fn drop(&mut self) {
        let _ = self.bin.set_state(gst::State::Null);
    }
}

async fn run(
    args: Args,
    initial_peers: &[&str],
    ws: impl Sink<WsMessage, Error = WsError> + Stream<Item = Result<WsMessage, WsError>>,
) -> Result<(), anyhow::Error> {
    // Split the websocket into the Sink and Stream
    let (mut ws_sink, ws_stream) = ws.split();
    // Fuse the Stream, required for the select macro
    let mut ws_stream = ws_stream.fuse();

    // Create our application state
    let (app, send_gst_msg_rx, send_ws_msg_rx) = App::new(args, initial_peers)?;

    let mut send_gst_msg_rx = send_gst_msg_rx.fuse();
    let mut send_ws_msg_rx = send_ws_msg_rx.fuse();

    // And now let's start our message loop
    loop {
        let ws_msg = futures::select! {
            // Handle the WebSocket messages here
            ws_msg = ws_stream.select_next_some() => {
                match ws_msg? {
                    WsMessage::Close(_) => {
                        println!("peer disconnected");
                        break
                    },
                    WsMessage::Ping(data) => Some(WsMessage::Pong(data)),
                    WsMessage::Pong(_) => None,
                    WsMessage::Binary(_) => None,
                    WsMessage::Text(text) => {
                        if let Err(err) = app.handle_websocket_message(&text) {
                            println!("Failed to parse message: {}", err);
                        }
                        None
                    },
                }
            },
            // Pass the GStreamer messages to the application control logic
            gst_msg = send_gst_msg_rx.select_next_some() => {
                app.handle_pipeline_message(&gst_msg)?;
                None
            },
            // Handle WebSocket messages we created asynchronously
            // to send them out now
            ws_msg = send_ws_msg_rx.select_next_some() => Some(ws_msg),
            // Once we're done, break the loop and return
            complete => break,
        };

        // If there's a message to send out, do so now
        if let Some(ws_msg) = ws_msg {
            ws_sink.send(ws_msg).await?;
        }
    }

    Ok(())
}

// Check if all GStreamer plugins we require are available
fn check_plugins() -> Result<(), anyhow::Error> {
    let needed = [
        "videotestsrc",
        "audiotestsrc",
        "videoconvert",
        "audioconvert",
        "autodetect",
        "opus",
        "vpx",
        "webrtc",
        "nice",
        "dtls",
        "srtp",
        "rtpmanager",
        "rtp",
        "playback",
        "videoscale",
        "audioresample",
        "compositor",
        "audiomixer",
    ];

    let registry = gst::Registry::get();
    let missing = needed
        .iter()
        .filter(|n| registry.find_plugin(n).is_none())
        .cloned()
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        bail!("Missing plugins: {:?}", missing);
    } else {
        Ok(())
    }
}

async fn async_main() -> Result<(), anyhow::Error> {
    // Initialize GStreamer first
    gst::init()?;

    check_plugins()?;

    let args = Args::from_args();

    // Connect to the given server
    let url = url::Url::parse(&args.server)?;
    let (mut ws, _) = async_tungstenite::connect_async(url).await?;

    println!("connected");

    // Say HELLO to the server and see if it replies with HELLO
    let our_id = rand::thread_rng().gen_range(10, 10_000);
    println!("Registering id {} with server", our_id);
    ws.send(WsMessage::Text(format!("HELLO {}", our_id)))
        .await?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))??;

    if msg != WsMessage::Text("HELLO".into()) {
        bail!("server didn't say HELLO");
    }

    // Join the given room
    ws.send(WsMessage::Text(format!("ROOM {}", args.room_id)))
        .await?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))??;

    let peers_str;
    if let WsMessage::Text(text) = &msg {
        if !text.starts_with("ROOM_OK") {
            bail!("server error: {:?}", text);
        }

        println!("Joined room {}", args.room_id);

        peers_str = &text["ROOM_OK ".len()..];
    } else {
        bail!("server error: {:?}", msg);
    }

    // Collect the ids of already existing peers
    let initial_peers = peers_str
        .split(' ')
        .filter_map(|p| {
            // Filter out empty lines
            let p = p.trim();
            if p.is_empty() {
                None
            } else {
                Some(p)
            }
        })
        .collect::<Vec<_>>();

    // All good, let's run our message loop
    run(args, &initial_peers, ws).await
}

fn main() -> Result<(), anyhow::Error> {
    macos_workaround::run(|| task::block_on(async_main()))
}
