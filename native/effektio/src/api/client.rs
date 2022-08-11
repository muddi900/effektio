use anyhow::{bail, Context, Result};
use derive_builder::Builder;
use effektio_core::{
    models::{Faq, News},
    statics::{PURPOSE_FIELD, PURPOSE_FIELD_DEV, PURPOSE_TEAM_VALUE},
    RestoreToken,
};

#[cfg(feature = "with-mocks")]
use effektio_core::mocks::gen_mock_faqs;
use futures::{
    channel::mpsc::{channel, Receiver},
    stream, Stream, StreamExt,
};
use futures_signals::signal::{
    channel as signal_channel, Receiver as SignalReceiver, SignalExt, SignalStream,
};
use log::info;
use matrix_sdk::{
    config::SyncSettings,
    locks::RwLock as MatrixRwLock,
    media::{MediaFormat, MediaRequest},
    ruma::{device_id, events::AnySyncRoomEvent, OwnedUserId, RoomId},
    Client as MatrixClient, LoopCtrl,
};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::{
    api::FfiBuffer,
    events::{handle_typing_notification, TypingNotification},
    Account, Conversation, DeviceListsController, Group, Room, SessionVerificationController, RUNTIME,
};

#[derive(Default, Builder, Debug)]
pub struct ClientState {
    #[builder(default)]
    pub is_guest: bool,
    #[builder(default)]
    pub has_first_synced: bool,
    #[builder(default)]
    pub is_syncing: bool,
    #[builder(default)]
    pub should_stop_syncing: bool,
}

#[derive(Clone)]
pub struct Client {
    pub(crate) client: MatrixClient,
    pub(crate) state: Arc<RwLock<ClientState>>,
    pub(crate) session_verification_controller:
        Arc<MatrixRwLock<Option<SessionVerificationController>>>,
    pub(crate) device_lists_controller: Arc<MatrixRwLock<Option<DeviceListsController>>>,
}

impl std::ops::Deref for Client {
    type Target = MatrixClient;
    fn deref(&self) -> &MatrixClient {
        &self.client
    }
}

pub(crate) async fn devide_groups_from_common(
    client: MatrixClient,
) -> (Vec<Group>, Vec<Conversation>) {
    let (groups, convos, _) = stream::iter(client.clone().rooms().into_iter())
        .fold(
            (Vec::new(), Vec::new(), client),
            async move |(mut groups, mut conversations, client), room| {
                let is_effektio_group = {
                    #[allow(clippy::match_like_matches_macro)]
                    if let Ok(Some(_)) = room
                        .get_state_event(PURPOSE_FIELD.into(), PURPOSE_TEAM_VALUE)
                        .await
                    {
                        true
                    } else if let Ok(Some(_)) = room
                        .get_state_event(PURPOSE_FIELD_DEV.into(), PURPOSE_TEAM_VALUE)
                        .await
                    {
                        true
                    } else {
                        false
                    }
                };

                if is_effektio_group {
                    groups.push(Group {
                        inner: Room {
                            room,
                            client: client.clone(),
                        },
                    });
                } else {
                    conversations.push(Conversation {
                        inner: Room {
                            room,
                            client: client.clone(),
                        },
                    });
                }

                (groups, conversations, client)
            },
        )
        .await;
    (groups, convos)
}

#[derive(Clone)]
pub struct SyncState {
    typing_notification_rx: Arc<Mutex<Option<Receiver<TypingNotification>>>>, // mutex for sync, arc for clone. once called, it will become None, not Some
    first_synced_rx: Arc<Mutex<Option<SignalReceiver<bool>>>>,
}

impl SyncState {
    pub fn new(
        typing_notification_rx: Receiver<TypingNotification>,
        first_synced_rx: SignalReceiver<bool>,
    ) -> Self {
        let typing_notification_rx = Arc::new(Mutex::new(Some(typing_notification_rx)));
        let first_synced_rx = Arc::new(Mutex::new(Some(first_synced_rx)));

        Self {
            typing_notification_rx,
            first_synced_rx,
        }
    }

    pub fn get_typing_notification_rx(&self) -> Option<Receiver<TypingNotification>> {
        self.typing_notification_rx.lock().take()
    }

    pub fn get_first_synced_rx(&self) -> Option<SignalStream<SignalReceiver<bool>>> {
        self.first_synced_rx.lock().take().map(|t| t.to_stream())
    }
}

impl Client {
    pub fn new(client: MatrixClient, state: ClientState) -> Self {
        Client {
            client,
            state: Arc::new(RwLock::new(state)),
            session_verification_controller: Arc::new(MatrixRwLock::new(None)),
            device_lists_controller: Arc::new(MatrixRwLock::new(None)),
        }
    }

    pub fn start_sync(&self) -> SyncState {
        let client = self.client.clone();
        let state = self.state.clone();
        let session_verification_controller = self.session_verification_controller.clone();
        let device_lists_controller = self.device_lists_controller.clone();

        let (typing_notification_tx, typing_notification_rx) = channel::<TypingNotification>(10); // dropping after more than 10 items queued
        let (first_synced_tx, first_synced_rx) = signal_channel(false);

        let typing_notification_arc = Arc::new(typing_notification_tx);
        let first_synced_arc = Arc::new(first_synced_tx);

        let initial_arc = Arc::new(AtomicBool::from(true));
        let sync_state = SyncState::new(
            typing_notification_rx,
            first_synced_rx,
        );

        RUNTIME.spawn(async move {
            let client = client.clone();
            let state = state.clone();
            let session_verification_controller = session_verification_controller.clone();
            let device_lists_controller = device_lists_controller.clone();

            client
                .clone()
                .sync_with_callback(SyncSettings::new(), move |response| {
                    let client = client.clone();
                    let state = state.clone();
                    let session_verification_controller = session_verification_controller.clone();
                    let device_lists_controller = device_lists_controller.clone();
                    let typing_notification_arc = typing_notification_arc.clone();
                    let first_synced_arc = first_synced_arc.clone();
                    let initial_arc = initial_arc.clone();

                    async move {
                        let state = state.clone();
                        let initial = initial_arc.clone();
                        let mut typing_notification_tx = (*typing_notification_arc).clone();

                        if let Some(dlc) = &*device_lists_controller.read().await {
                            dlc.process_events(&client, response.device_lists);
                        }

                        if !initial.load(Ordering::SeqCst) {
                            if let Some(svc) = &*session_verification_controller.read().await {
                                svc.process_sync_messages(&client, &response.rooms);
                            }
                            for (room_id, room_info) in response.rooms.join {
                                for event in room_info.ephemeral.events {
                                    if let Ok(ev) = event.deserialize() {
                                        handle_typing_notification(
                                            &room_id,
                                            &ev,
                                            &client,
                                            &mut typing_notification_tx,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }

                        initial.store(false, Ordering::SeqCst);

                        let _ = first_synced_arc.send(true);
                        if !(*state).read().has_first_synced {
                            (*state).write().has_first_synced = true
                        }
                        if (*state).read().should_stop_syncing {
                            (*state).write().is_syncing = false;
                            // the lock is unlocked here when `s` goes out of scope.
                            return LoopCtrl::Break;
                        } else if !(*state).read().is_syncing {
                            (*state).write().is_syncing = true;
                        }

                        if let Some(svc) = &*session_verification_controller.read().await {
                            svc.process_to_device_messages(&client, response.to_device);
                        }
                        // the lock is unlocked here when `s` goes out of scope.
                        LoopCtrl::Continue
                    }
                })
                .await;
        });
        sync_state
    }

    /// Indication whether we've received a first sync response since
    /// establishing the client (in memory)
    pub fn has_first_synced(&self) -> bool {
        self.state.read().has_first_synced
    }

    /// Indication whether we are currently syncing
    pub fn is_syncing(&self) -> bool {
        self.state.read().is_syncing
    }

    /// Is this a guest account?
    pub fn is_guest(&self) -> bool {
        self.state.read().is_guest
    }

    pub async fn restore_token(&self) -> Result<String> {
        let session = self.client.session().context("Missing session")?.clone();
        let homeurl = self.client.homeserver().await;
        Ok(serde_json::to_string(&RestoreToken {
            session,
            homeurl,
            is_guest: self.state.read().is_guest,
        })?)
    }

    pub async fn conversations(&self) -> Result<Vec<Conversation>> {
        let c = self.client.clone();
        RUNTIME
            .spawn(async move {
                let (_, conversations) = devide_groups_from_common(c).await;
                Ok(conversations)
            })
            .await?
    }

    #[cfg(feature = "with-mocks")]
    pub async fn faqs(&self) -> Result<Vec<Faq>> {
        Ok(gen_mock_faqs())
    }

    // pub async fn get_mxcuri_media(&self, uri: String) -> Result<Vec<u8>> {
    //     let l = self.client.clone();
    //     RUNTIME.spawn(async move {
    //         let user_id = l.user_id().await.expect("No User ID found");
    //         Ok(user_id.as_str().to_string())
    //     }).await?
    // }

    pub async fn user_id(&self) -> Result<OwnedUserId> {
        let l = self.client.clone();
        RUNTIME
            .spawn(async move {
                let user_id = l.user_id().context("No User ID found")?.to_owned();
                Ok(user_id)
            })
            .await?
    }

    pub async fn room(&self, room_name: String) -> Result<Room> {
        let room_id = RoomId::parse(room_name)?;
        let l = self.client.clone();
        RUNTIME
            .spawn(async move {
                if let Some(room) = l.get_room(&room_id) {
                    return Ok(Room {
                        room,
                        client: l.clone(),
                    });
                }
                bail!("Room not found")
            })
            .await?
    }

    pub async fn account(&self) -> Result<Account> {
        Ok(Account::new(self.client.account()))
    }

    pub async fn display_name(&self) -> Result<String> {
        let l = self.client.clone();
        RUNTIME
            .spawn(async move {
                let display_name = l
                    .account()
                    .get_display_name()
                    .await?
                    .context("No User ID found")?;
                Ok(display_name.as_str().to_string())
            })
            .await?
    }

    pub async fn device_id(&self) -> Result<String> {
        let l = self.client.clone();
        RUNTIME
            .spawn(async move {
                let device_id = l.device_id().context("No Device ID found")?;
                Ok(device_id.as_str().to_string())
            })
            .await?
    }

    pub async fn avatar(&self) -> Result<FfiBuffer<u8>> {
        self.account().await?.avatar().await
    }

    pub async fn verified_device(&self, dev_id: String) -> Result<bool> {
        let c = self.client.clone();
        RUNTIME
            .spawn(async move {
                let user_id = c.user_id().expect("guest user cannot request verification");
                let dev = c
                    .encryption()
                    .get_device(user_id, device_id!(dev_id.as_str()))
                    .await
                    .expect("client should get device")
                    .unwrap();
                Ok(dev.verified())
            })
            .await?
    }

    pub async fn get_session_verification_controller(
        &self,
    ) -> Result<SessionVerificationController> {
        // if not exists, create new controller and return it.
        // thus Result is necessary but Option is not necessary.
        let c = self.client.clone();
        let session_verification_controller = self.session_verification_controller.clone();
        RUNTIME
            .spawn(async move {
                if let Some(svc) = &*session_verification_controller.read().await {
                    return Ok(svc.clone());
                }
                let svc = SessionVerificationController::new();
                *session_verification_controller.write().await = Some(svc.clone());
                Ok(svc)
            })
            .await?
    }

    pub async fn get_device_lists_controller(
        &self,
    ) -> Result<DeviceListsController> {
        // if not exists, create new controller and return it.
        // thus Result is necessary but Option is not necessary.
        let c = self.client.clone();
        let device_lists_controller = self.device_lists_controller.clone();
        RUNTIME
            .spawn(async move {
                if let Some(dlc) = &*device_lists_controller.read().await {
                    return Ok(dlc.clone());
                }
                let dlc = DeviceListsController::new();
                *device_lists_controller.write().await = Some(dlc.clone());
                Ok(dlc)
            })
            .await?
    }
}
