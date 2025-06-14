use std::borrow::Cow;
use std::cmp::min;
use std::collections::{HashSet, VecDeque};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::hqm_behaviour::HQMServerBehaviour;
use crate::hqm_simulate::{limit_vector_length, HQMSimulationEvent};
use bytes::{BufMut, BytesMut};
use chrono::{DateTime, Utc};
use nalgebra::{Point3, Rotation3, Vector3};
use std::fmt;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use uuid::Uuid;

use async_stream::stream;
use futures::StreamExt;

use crate::hqm_game::{
    HQMGameValues, HQMGameWorld, HQMObjectIndex, HQMPhysicsConfiguration, HQMPlayerInput,
    HQMRulesState, HQMSkater, HQMSkaterHand,
};
use crate::hqm_parse;
use crate::hqm_parse::{
    write_message, write_objects, HQMClientToServerMessage, HQMMessageCodec, HQMMessageWriter,
    HQMObjectPacket,
};

pub(crate) const GAME_HEADER: &[u8] = b"Hock";

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum HQMClientVersion {
    Vanilla,
    Ping,
    PingRules,
}

impl HQMClientVersion {
    pub(crate) fn has_ping(self) -> bool {
        match self {
            HQMClientVersion::Vanilla => false,
            HQMClientVersion::Ping => true,
            HQMClientVersion::PingRules => true,
        }
    }

    pub(crate) fn has_rules(self) -> bool {
        match self {
            HQMClientVersion::Vanilla => false,
            HQMClientVersion::Ping => false,
            HQMClientVersion::PingRules => true,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct HQMServerPlayerIndex(pub usize);

impl std::fmt::Display for HQMServerPlayerIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for HQMServerPlayerIndex {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(HQMServerPlayerIndex)
    }
}

pub struct HQMServerPlayerList {
    players: Vec<Option<HQMServerPlayer>>,
}

impl HQMServerPlayerList {
    pub fn iter(&self) -> impl Iterator<Item = (HQMServerPlayerIndex, &HQMServerPlayer)> {
        self.players
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.as_ref().map(|p| (HQMServerPlayerIndex(i), p)))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (HQMServerPlayerIndex, &mut HQMServerPlayer)> {
        self.players
            .iter_mut()
            .enumerate()
            .filter_map(|(i, p)| p.as_mut().map(|p| (HQMServerPlayerIndex(i), p)))
    }

    pub fn get(
        &self,
        HQMServerPlayerIndex(player_index): HQMServerPlayerIndex,
    ) -> Option<&HQMServerPlayer> {
        if let Some(x) = self.players.get(player_index) {
            x.as_ref()
        } else {
            None
        }
    }

    pub(crate) fn get_mut(
        &mut self,
        HQMServerPlayerIndex(player_index): HQMServerPlayerIndex,
    ) -> Option<&mut HQMServerPlayer> {
        if let Some(x) = self.players.get_mut(player_index) {
            x.as_mut()
        } else {
            None
        }
    }

    pub fn get_from_object_index(
        &mut self,
        object_index: HQMObjectIndex,
    ) -> Option<(HQMServerPlayerIndex, HQMTeam, &HQMServerPlayer)> {
        for (player_index, player) in self.players.iter().enumerate() {
            if let Some(player) = player {
                if let Some((o, team)) = player.object {
                    if o == object_index {
                        return Some((HQMServerPlayerIndex(player_index), team, player));
                    }
                }
            }
        }
        None
    }

    fn remove_player(&mut self, HQMServerPlayerIndex(player_index): HQMServerPlayerIndex) {
        self.players[player_index] = None;
    }

    fn add_player(
        &mut self,
        HQMServerPlayerIndex(player_index): HQMServerPlayerIndex,
        player: HQMServerPlayer,
    ) {
        self.players[player_index] = Some(player);
    }
}

enum HQMWaitingMessageReceiver {
    All,
    Specific(HQMServerPlayerIndex),
}

#[derive(Debug, Clone)]
pub enum HQMMessage {
    PlayerUpdate {
        player_name: Rc<String>,
        object: Option<(HQMObjectIndex, HQMTeam)>,
        player_index: HQMServerPlayerIndex,
        in_server: bool,
    },
    Goal {
        team: HQMTeam,
        goal_player_index: Option<HQMServerPlayerIndex>,
        assist_player_index: Option<HQMServerPlayerIndex>,
    },
    Chat {
        player_index: Option<HQMServerPlayerIndex>,
        message: Cow<'static, str>,
    },
}

pub struct HQMServerMessages {
    persistent_messages: Vec<Rc<HQMMessage>>,
    replay_messages: Vec<Rc<HQMMessage>>,
    waiting_messages: Vec<(HQMWaitingMessageReceiver, Rc<HQMMessage>)>,
}

impl HQMServerMessages {
    fn new() -> Self {
        Self {
            persistent_messages: Vec::with_capacity(1024),
            replay_messages: Vec::with_capacity(1024),
            waiting_messages: Vec::with_capacity(64),
        }
    }

    fn clear(&mut self) {
        self.persistent_messages.clear();
        self.replay_messages.clear();
        self.waiting_messages.clear();
    }

    pub fn add_user_chat_message(
        &mut self,
        message: impl Into<Cow<'static, str>>,
        sender_index: HQMServerPlayerIndex,
    ) {
        let chat = HQMMessage::Chat {
            player_index: Some(sender_index),
            message: message.into(),
        };
        self.add_global_message(chat, false, true);
    }

    pub fn add_server_chat_message(&mut self, message: impl Into<Cow<'static, str>>) {
        let chat = HQMMessage::Chat {
            player_index: None,
            message: message.into(),
        };
        self.add_global_message(chat, false, true);
    }

    pub fn add_directed_chat_message(
        &mut self,
        message: impl Into<Cow<'static, str>>,
        receiver_index: HQMServerPlayerIndex,
        sender_index: Option<HQMServerPlayerIndex>,
    ) {
        let chat = HQMMessage::Chat {
            player_index: sender_index,
            message: message.into(),
        };
        self.add_directed_message(chat, receiver_index);
    }

    pub fn add_directed_user_chat_message(
        &mut self,
        message: impl Into<Cow<'static, str>>,
        receiver_index: HQMServerPlayerIndex,
        sender_index: HQMServerPlayerIndex,
    ) {
        self.add_directed_chat_message(message, receiver_index, Some(sender_index));
    }

    pub fn add_directed_server_chat_message(
        &mut self,
        message: impl Into<Cow<'static, str>>,
        receiver_index: HQMServerPlayerIndex,
    ) {
        self.add_directed_chat_message(message, receiver_index, None);
    }

    pub fn add_goal_message(
        &mut self,
        team: HQMTeam,
        goal_player_index: Option<HQMServerPlayerIndex>,
        assist_player_index: Option<HQMServerPlayerIndex>,
    ) {
        let message = HQMMessage::Goal {
            team,
            goal_player_index,
            assist_player_index,
        };
        self.add_global_message(message, true, true);
    }

    fn add_global_message(&mut self, message: HQMMessage, persistent: bool, replay: bool) {
        let rc = Rc::new(message);
        if replay {
            self.replay_messages.push(rc.clone());
        }
        if persistent {
            self.persistent_messages.push(rc.clone());
        }
        self.waiting_messages
            .push((HQMWaitingMessageReceiver::All, rc));
    }

    fn add_directed_message(&mut self, message: HQMMessage, receiver: HQMServerPlayerIndex) {
        let rc = Rc::new(message);
        self.waiting_messages
            .push((HQMWaitingMessageReceiver::Specific(receiver), rc));
    }
}

pub struct HQMServer {
    pub players: HQMServerPlayerList,
    pub messages: HQMServerMessages,
    pub(crate) ban_list: HashSet<std::net::IpAddr>,
    pub(crate) allow_join: bool,
    pub config: HQMServerConfiguration,
    pub values: HQMGameValues,
    pub world: HQMGameWorld,
    replay_queue: VecDeque<ReplayElement>,
    requested_replays: VecDeque<(u32, u32, Option<HQMServerPlayerIndex>)>,
    game_id: u32,
    pub game_step: u32,
    pub is_muted: bool,
    pub start_time: DateTime<Utc>,
    reqwest_client: reqwest::Client,

    has_current_game_been_active: bool,

    packet: u32,
    replay_data: BytesMut,
    replay_msg_pos: usize,
    replay_last_packet: u32,

    saved_packets: VecDeque<[HQMObjectPacket; 32]>,
    saved_pings: VecDeque<Instant>,
    saved_history: VecDeque<ReplayTick>,

    pub history_length: usize,

    saved_events: VecDeque<(u32, VecDeque<(HQMObjectIndex, HQMObjectIndex)>)>,
}

impl HQMServer {
    async fn handle_message<B: HQMServerBehaviour>(
        &mut self,
        addr: SocketAddr,
        socket: &Arc<UdpSocket>,
        command: HQMClientToServerMessage,
        behaviour: &mut B,
        write_buf: &mut BytesMut,
    ) {
        match command {
            HQMClientToServerMessage::Join {
                version,
                player_name,
            } => {
                self.player_join(addr, version, player_name, behaviour);
            }
            HQMClientToServerMessage::Update {
                current_game_id,
                input,
                deltatime,
                new_known_packet,
                known_msg_pos,
                chat,
                version,
            } => self.player_update(
                addr,
                current_game_id,
                input,
                deltatime,
                new_known_packet,
                known_msg_pos,
                chat,
                version,
                behaviour,
            ),
            HQMClientToServerMessage::Exit => self.player_exit(addr, behaviour),
            HQMClientToServerMessage::ServerInfo { version, ping } => {
                self.request_info(socket, addr, version, ping, behaviour, write_buf)
                    .await;
            }
        }
    }

    async fn request_info<'a, B: HQMServerBehaviour>(
        &self,
        socket: &Arc<UdpSocket>,
        addr: SocketAddr,
        _version: u32,
        ping: u32,
        behaviour: &B,
        write_buf: &mut BytesMut,
    ) {
        write_buf.clear();
        let mut writer = HQMMessageWriter::new(write_buf);
        writer.write_bytes_aligned(GAME_HEADER);
        writer.write_byte_aligned(1);
        writer.write_bits(8, 55);
        writer.write_u32_aligned(ping);

        let player_count = self.player_count();
        writer.write_bits(8, player_count as u32);
        writer.write_bits(4, 4);
        writer.write_bits(4, behaviour.get_number_of_players() as u32);

        writer.write_bytes_aligned_padded(32, self.config.server_name.as_ref());

        let socket = socket.clone();
        let addr = addr.clone();

        let slice: &[u8] = &write_buf;
        let _ = socket.send_to(slice, addr).await;
    }

    fn player_count(&self) -> usize {
        let mut player_count = 0;
        for (_, player) in self.players.iter() {
            let is_actual_player = match player.data {
                HQMServerPlayerData::NetworkPlayer { .. } => true,
            };
            if is_actual_player {
                player_count += 1;
            }
        }
        player_count
    }

    fn player_update<B: HQMServerBehaviour>(
        &mut self,
        addr: SocketAddr,
        current_game_id: u32,
        input: HQMPlayerInput,
        deltatime: Option<u32>,
        new_known_packet: u32,
        known_msgpos: usize,
        chat: Option<(u8, String)>,
        client_version: HQMClientVersion,
        behaviour: &mut B,
    ) {
        let current_slot = self.find_player_slot(addr);
        let (player_index, player) = match current_slot {
            Some(x) => (x, self.players.get_mut(x).unwrap()),
            None => {
                return;
            }
        };
        if let HQMServerPlayerData::NetworkPlayer { data } = &mut player.data {
            let time_received = Instant::now();

            let duration_since_packet =
                if data.game_id == current_game_id && data.known_packet < new_known_packet {
                    let ticks = &self.saved_pings;
                    self.packet
                        .checked_sub(new_known_packet)
                        .and_then(|diff| ticks.get(diff as usize))
                        .and_then(|last_time_received| {
                            time_received.checked_duration_since(*last_time_received)
                        })
                } else {
                    None
                };

            if let Some(duration_since_packet) = duration_since_packet {
                data.last_ping.truncate(100 - 1);
                data.last_ping
                    .push_front(duration_since_packet.as_secs_f32());
            }

            data.inactivity = 0;
            data.client_version = client_version;
            data.known_packet = new_known_packet;
            player.input = input;
            data.game_id = current_game_id;
            data.known_msgpos = known_msgpos;

            if let Some(deltatime) = deltatime {
                data.deltatime = deltatime;
            }

            if let Some((rep, message)) = chat {
                if data.chat_rep != Some(rep) {
                    data.chat_rep = Some(rep);
                    self.process_message(message, player_index, behaviour);
                }
            }
        }
    }

    fn player_join<B: HQMServerBehaviour>(
        &mut self,
        addr: SocketAddr,
        player_version: u32,
        name: String,
        behaviour: &mut B,
    ) {
        let player_count = self.player_count();
        let max_player_count = self.config.player_max;
        if player_count >= max_player_count {
            return; // Ignore join request
        }
        if player_version != 55 {
            return; // Not the right version
        }
        let current_slot = self.find_player_slot(addr);
        if current_slot.is_some() {
            return; // Player has already joined
        }

        // Check ban list
        if self.ban_list.contains(&addr.ip()) {
            return;
        }

        // Disabled join
        if !self.allow_join {
            return;
        }

        if let Some(player_index) = self.add_player(name.clone(), addr) {
            behaviour.after_player_join(self, player_index);
            info!(
                "{} ({}) joined server from address {:?}",
                name, player_index, addr
            );
            let msg = format!("{} joined", name);
            self.messages.add_server_chat_message(msg);
        }
    }

    pub fn set_hand(&mut self, hand: HQMSkaterHand, player_index: HQMServerPlayerIndex) {
        if let Some(player) = self.players.get_mut(player_index) {
            player.hand = hand;
            let object_index = player.object.map(|x| x.0);

            fn change_skater(
                server: &mut HQMServer,
                object_index: HQMObjectIndex,
                msg_player_index: HQMServerPlayerIndex,
                hand: HQMSkaterHand,
            ) {
                if let Some(skater) = server.world.objects.get_skater_mut(object_index) {
                    if server.values.period != 0 {
                        server.messages.add_directed_server_chat_message(
                            "Stick hand will change after next intermission",
                            msg_player_index,
                        );

                        return;
                    }

                    skater.hand = hand;
                }
            }

            if let Some(object_index) = object_index {
                change_skater(self, object_index, player_index, hand);
            }
        }
    }

    fn process_command<B: HQMServerBehaviour>(
        &mut self,
        command: &str,
        arg: &str,
        player_index: HQMServerPlayerIndex,
        behaviour: &mut B,
    ) {
        match command {
            "enablejoin" => {
                self.set_allow_join(player_index, true);
            }
            "disablejoin" => {
                self.set_allow_join(player_index, false);
            }
            "mute" => {
                if let Ok(mute_player_index) = arg.parse::<HQMServerPlayerIndex>() {
                    self.mute_player(player_index, mute_player_index);
                }
            }
            "unmute" => {
                if let Ok(mute_player_index) = arg.parse::<HQMServerPlayerIndex>() {
                    self.unmute_player(player_index, mute_player_index);
                }
            }
            /*"shadowmute" => {
                if let Ok(mute_player_index) = arg.parse::<usize>() {
                    if mute_player_index < self.players.len() {
                        self.shadowmute_player(player_index, mute_player_index);
                    }
                }
            },*/
            "mutechat" => {
                self.mute_chat(player_index);
            }
            "unmutechat" => {
                self.unmute_chat(player_index);
            }
            "kick" => {
                if let Ok(kick_player_index) = arg.parse::<HQMServerPlayerIndex>() {
                    self.kick_player(player_index, kick_player_index, false, behaviour);
                }
            }
            "kickall" => {
                self.kick_all_matching(player_index, arg, false, behaviour);
            }
            "ban" => {
                if let Ok(kick_player_index) = arg.parse::<HQMServerPlayerIndex>() {
                    self.kick_player(player_index, kick_player_index, true, behaviour);
                }
            }
            "banall" => {
                self.kick_all_matching(player_index, arg, true, behaviour);
            }
            "clearbans" => {
                self.clear_bans(player_index);
            }
            "lefty" => {
                self.set_hand(HQMSkaterHand::Left, player_index);
            }
            "righty" => {
                self.set_hand(HQMSkaterHand::Right, player_index);
            }
            "admin" => {
                self.admin_login(player_index, arg);
            }
            "serverrestart" => {
                self.restart_server(player_index);
            }
            "list" => {
                if arg.is_empty() {
                    self.list_players(player_index, 0);
                } else if let Ok(first_index) = arg.parse::<usize>() {
                    self.list_players(player_index, first_index);
                }
            }
            "search" => {
                self.search_players(player_index, arg);
            }
            "ping" => {
                if let Ok(ping_player_index) = arg.parse::<HQMServerPlayerIndex>() {
                    self.ping(ping_player_index, player_index);
                }
            }
            "pings" => {
                if let Some((ping_player_index, _name)) = self.player_exact_unique_match(arg) {
                    self.ping(ping_player_index, player_index);
                } else {
                    let matches = self.player_search(arg);
                    if matches.is_empty() {
                        self.messages
                            .add_directed_server_chat_message("No matches found", player_index);
                    } else if matches.len() > 1 {
                        self.messages.add_directed_server_chat_message(
                            "Multiple matches found, use /ping X",
                            player_index,
                        );
                        for (found_player_index, found_player_name) in matches.into_iter().take(5) {
                            let msg = format!("{}: {}", found_player_index, found_player_name);
                            self.messages
                                .add_directed_server_chat_message(msg, player_index);
                        }
                    } else {
                        self.ping(matches[0].0, player_index);
                    }
                }
            }
            "view" => {
                if let Ok(view_player_index) = arg.parse::<HQMServerPlayerIndex>() {
                    self.view(view_player_index, player_index);
                }
            }
            "views" => {
                if let Some((view_player_index, _name)) = self.player_exact_unique_match(arg) {
                    self.view(view_player_index, player_index);
                } else {
                    let matches = self.player_search(arg);
                    if matches.is_empty() {
                        self.messages
                            .add_directed_server_chat_message("No matches found", player_index);
                    } else if matches.len() > 1 {
                        self.messages.add_directed_server_chat_message(
                            "Multiple matches found, use /view X",
                            player_index,
                        );
                        for (found_player_index, found_player_name) in matches.into_iter().take(5) {
                            let str = format!("{}: {}", found_player_index, found_player_name);
                            self.messages
                                .add_directed_server_chat_message(str, player_index);
                        }
                    } else {
                        self.view(matches[0].0, player_index);
                    }
                }
            }
            "restoreview" => {
                if let Some(player) = self.players.get_mut(player_index) {
                    if let HQMServerPlayerData::NetworkPlayer { data } = &mut player.data {
                        if data.view_player_index != player_index {
                            data.view_player_index = player_index;
                            self.messages.add_directed_server_chat_message(
                                "View has been restored",
                                player_index,
                            );
                        }
                    }
                }
            }
            "t" => {
                self.add_user_team_message(arg, player_index);
            }
        "lm" => {
            // Блокируем смену stick limit'а — всегда принудительно "no"
            self.messages.add_directed_server_chat_message(
                "Stick limit is locked to 'no' on this server.".to_string(),
                player_index,
            );
        }
        _ => behaviour.handle_command(self, command, arg, player_index),
    }
}

    fn set_stick_limit(&mut self, _limit: f32, player_index: HQMServerPlayerIndex) {
    let limit = 0.0;

    if let Some(player) = self.players.get_mut(player_index) {
        player.stick_limit = 0.0;

        if let Some((object_index, _)) = player.object {
            if let Some(skater) = self.world.objects.get_skater_mut(object_index) {
                skater.stick_limit = 0.0;
            }
        }

        let msg = "Stick speed limit is set to no".to_string();
        self.messages
            .add_directed_server_chat_message(msg, player_index);
    }
}

    fn list_players(&mut self, receiver_index: HQMServerPlayerIndex, first_index: usize) {
        for (player_index, player) in self
            .players
            .iter()
            .filter(|(x, _)| x.0 >= first_index)
            .take(5)
        {
            let msg = format!("{}: {}", player_index, player.player_name);
            self.messages
                .add_directed_server_chat_message(msg, receiver_index);
        }
    }

    fn search_players(&mut self, player_index: HQMServerPlayerIndex, name: &str) {
        let matches = self.player_search(name);
        if matches.is_empty() {
            self.messages
                .add_directed_server_chat_message("No matches found", player_index);
            return;
        }
        for (found_player_index, found_player_name) in matches.into_iter().take(5) {
            let msg = format!("{}: {}", found_player_index, found_player_name);
            self.messages
                .add_directed_server_chat_message(msg, player_index);
        }
    }

    fn view(
        &mut self,
        view_player_index: HQMServerPlayerIndex,
        player_index: HQMServerPlayerIndex,
    ) {
        if let Some(view_player) = self.players.get(view_player_index) {
            let view_player_name = view_player.player_name.clone();

            if let Some(player) = self.players.get_mut(player_index) {
                if let HQMServerPlayerData::NetworkPlayer { data } = &mut player.data {
                    if player.object.is_some() {
                        self.messages.add_directed_server_chat_message(
                            "You must be a spectator to change view",
                            player_index,
                        );
                    } else if view_player_index != data.view_player_index {
                        data.view_player_index = view_player_index;
                        if player_index != view_player_index {
                            let msg = format!("You are now viewing {}", view_player_name);
                            self.messages
                                .add_directed_server_chat_message(msg, player_index);
                        } else {
                            self.messages.add_directed_server_chat_message(
                                "View has been restored",
                                player_index,
                            );
                        }
                    }
                }
            }
        } else {
            self.messages
                .add_directed_server_chat_message("No player with this ID exists", player_index);
        }
    }

    fn ping(
        &mut self,
        ping_player_index: HQMServerPlayerIndex,
        player_index: HQMServerPlayerIndex,
    ) {
        if let Some(ping_player) = self.players.get(ping_player_index) {
            if let Some(ping) = ping_player.ping_data() {
                let msg1 = format!(
                    "{} ping: avg {:.0} ms",
                    ping_player.player_name,
                    (ping.avg * 1000f32)
                );
                let msg2 = format!(
                    "min {:.0} ms, max {:.0} ms, std.dev {:.1}",
                    (ping.min * 1000f32),
                    (ping.max * 1000f32),
                    (ping.deviation * 1000f32)
                );
                self.messages
                    .add_directed_server_chat_message(msg1, player_index);
                self.messages
                    .add_directed_server_chat_message(msg2, player_index);
            } else {
                self.messages.add_directed_server_chat_message(
                    "This player is not a connected player",
                    player_index,
                );
            }
        } else {
            self.messages
                .add_directed_server_chat_message("No player with this ID exists", player_index);
        }
    }

    pub fn player_exact_unique_match(
        &self,
        name: &str,
    ) -> Option<(HQMServerPlayerIndex, Rc<String>)> {
        let mut found = None;
        for (player_index, player) in self.players.iter() {
            if player.player_name.as_str() == name {
                if found.is_none() {
                    found = Some((player_index, player.player_name.clone()));
                } else {
                    return None;
                }
            }
        }
        found
    }

    pub fn player_search(
        &self,
        name: &str,
    ) -> smallvec::SmallVec<[(HQMServerPlayerIndex, Rc<String>); 64]> {
        let name = name.to_lowercase();
        let mut found = smallvec::SmallVec::<[_; 64]>::new();
        for (player_index, player) in self.players.iter() {
            if player.player_name.to_lowercase().contains(&name) {
                found.push((player_index, player.player_name.clone()));
                if found.len() >= 5 {
                    break;
                }
            }
        }
        found
    }

    fn process_message<B: HQMServerBehaviour>(
        &mut self,
        msg: String,
        player_index: HQMServerPlayerIndex,
        behaviour: &mut B,
    ) {
        if self.players.get(player_index).is_some() {
            if msg.starts_with("/") {
                let split: Vec<&str> = msg.splitn(2, " ").collect();
                let command = &split[0][1..];
                let arg = if split.len() < 2 { "" } else { &split[1] };
                self.process_command(command, arg, player_index, behaviour);
            } else {
                if !self.is_muted {
                    match self.players.get(player_index) {
                        Some(player) => match player.is_muted {
                            HQMMuteStatus::NotMuted => {
                                info!("{} ({}): {}", &player.player_name, player_index, &msg);
                                self.messages.add_user_chat_message(msg, player_index);
                            }
                            HQMMuteStatus::ShadowMuted => {
                                self.messages.add_directed_user_chat_message(
                                    msg,
                                    player_index,
                                    player_index,
                                );
                            }
                            HQMMuteStatus::Muted => {}
                        },
                        _ => {
                            return;
                        }
                    }
                }
            }
        }
    }

    fn player_exit<B: HQMServerBehaviour>(&mut self, addr: SocketAddr, behaviour: &mut B) {
        let player_index = self.find_player_slot(addr);

        if let Some(player_index) = player_index {
            let player_name = {
                let player = self.players.get(player_index).unwrap();
                player.player_name.clone()
            };
            behaviour.before_player_exit(self, player_index);
            self.remove_player(player_index, true);
            info!("{} ({}) exited server", player_name, player_index);
            let msg = format!("{} exited", player_name);
            self.messages.add_server_chat_message(msg);
        }
    }

    fn add_player(
        &mut self,
        player_name: String,
        addr: SocketAddr,
    ) -> Option<HQMServerPlayerIndex> {
        let player_index = self.find_empty_player_slot();
        match player_index {
            Some(player_index) => {
                let new_player = HQMServerPlayer::new_network_player(
                    player_index,
                    player_name,
                    addr,
                    &self.messages.persistent_messages,
                );
                let update = new_player.get_update_message(player_index);

                self.players.add_player(player_index, new_player);

                self.messages.add_global_message(update, true, true);

                let welcome = self.config.welcome.clone();
                for welcome_msg in welcome {
                    self.messages
                        .add_directed_server_chat_message(welcome_msg, player_index);
                }

                Some(player_index)
            }
            _ => None,
        }
    }

    pub fn remove_player(&mut self, player_index: HQMServerPlayerIndex, on_replay: bool) {
        if let Some(player) = self.players.get(player_index) {
            let player_name = player.player_name.clone();
            let is_admin = player.is_admin;

            if let Some((object_index, _)) = player.object {
                self.world.remove_player(object_index);
            }

            let update = HQMMessage::PlayerUpdate {
                player_name,
                object: None,
                player_index,
                in_server: false,
            };

            self.messages.add_global_message(update, true, on_replay);

            self.players.remove_player(player_index);

            if is_admin {
                let admin_found = self.players.iter().any(|(_, x)| x.is_admin);

                if !admin_found {
                    self.allow_join = true;
                }
            }
        }
    }

    pub fn move_to_spectator(&mut self, player_index: HQMServerPlayerIndex) -> bool {
        if let Some(player) = self.players.get_mut(player_index) {
            if let Some((object_index, _)) = player.object {
                if self.world.remove_player(object_index) {
                    player.object = None;
                    let update = player.get_update_message(player_index);
                    self.messages.add_global_message(update, true, true);

                    return true;
                }
            }
        }
        false
    }

    pub fn spawn_skater(
        &mut self,
        player_index: HQMServerPlayerIndex,
        team: HQMTeam,
        pos: Point3<f32>,
        rot: Rotation3<f32>,
        keep_stick_position: bool,
    ) -> Option<HQMObjectIndex> {
        if let Some(player) = self.players.get_mut(player_index) {
            if let Some((object_index, _)) = player.object {
                if let Some(skater) = self.world.objects.get_skater_mut(object_index) {
                    let mut new_skater =
                        HQMSkater::new(pos, rot, player.hand, player.mass, player.stick_limit);
                    if keep_stick_position {
                        let stick_pos_diff = &skater.stick_pos - &skater.body.pos;
                        let rot_change = skater.body.rot.rotation_to(&rot);
                        let stick_rot_diff = skater.body.rot.rotation_to(&skater.stick_rot);

                        new_skater.stick_pos = pos + (rot_change * stick_pos_diff);
                        new_skater.stick_rot = &stick_rot_diff * &rot;
                        new_skater.stick_placement = skater.stick_placement;
                    }
                    *skater = new_skater;
                    let object = Some((object_index, team));
                    player.object = object;
                    let update = player.get_update_message(player_index);
                    self.messages.add_global_message(update, true, true);
                }
            } else {
                if let Some(skater) = self.world.create_player_object(
                    pos,
                    rot,
                    player.hand,
                    player.mass,
                    player.stick_limit,
                ) {
                    if let HQMServerPlayerData::NetworkPlayer { data } = &mut player.data {
                        data.view_player_index = player_index;
                    }

                    let object = Some((skater, team));
                    player.object = object;
                    let update = player.get_update_message(player_index);
                    self.messages.add_global_message(update, true, true);
                    return Some(skater);
                }
            }
        }
        None
    }

    fn add_user_team_message(&mut self, message: &str, sender_index: HQMServerPlayerIndex) {
        if let Some(player) = self.players.get(sender_index) {
            let team = if let Some((_, team)) = player.object {
                Some(team)
            } else {
                None
            };
            if let Some(team) = team {
                info!(
                    "{} ({}) to team {}: {}",
                    &player.player_name, sender_index, team, message
                );

                let change1 = Rc::new(HQMMessage::PlayerUpdate {
                    player_name: Rc::new(format!("[{}] {}", team, player.player_name)),
                    object: player.object,
                    player_index: sender_index,
                    in_server: true,
                });
                let change2 = Rc::new(HQMMessage::PlayerUpdate {
                    player_name: player.player_name.clone(),
                    object: player.object,
                    player_index: sender_index,
                    in_server: true,
                });
                let chat = Rc::new(HQMMessage::Chat {
                    player_index: Some(sender_index),
                    message: Cow::Owned(message.to_owned()),
                });

                let mut matching_indices = smallvec::SmallVec::<[_; 32]>::new();
                for (player_index, player) in self.players.iter() {
                    if let Some((_, player_team)) = player.object {
                        if player_team == team {
                            matching_indices.push(player_index);
                        }
                    }
                }
                for player_index in matching_indices {
                    if let Some(player) = self.players.get_mut(player_index) {
                        player.add_message(change1.clone());
                        player.add_message(chat.clone());
                        player.add_message(change2.clone());
                    }
                }
            }
        }
    }

    fn find_player_slot(&self, addr: SocketAddr) -> Option<HQMServerPlayerIndex> {
        return self
            .players
            .iter()
            .find(|(_, x)| {
                if let HQMServerPlayerData::NetworkPlayer { data } = &x.data {
                    data.addr == addr
                } else {
                    false
                }
            })
            .map(|x| x.0);
    }

    fn find_empty_player_slot(&self) -> Option<HQMServerPlayerIndex> {
        return self
            .players
            .players
            .iter()
            .position(|x| x.is_none())
            .map(HQMServerPlayerIndex);
    }

    fn game_step<B: HQMServerBehaviour>(&mut self, behaviour: &mut B) {
        self.game_step = self.game_step.wrapping_add(1);

        behaviour.before_tick(self);

        for (_, player) in self.players.iter() {
            if let Some((object_index, _)) = player.object {
                if let Some(skater) = self.world.objects.get_skater_mut(object_index) {
                    skater.input = player.input.clone()
                }
            }
        }

        let events = self.world.simulate_step();

        let temp_events = events.clone();

        self.saved_events.truncate(3 - 1);

        let mut step_events = VecDeque::new();

        for event in temp_events {
            match event {
                HQMSimulationEvent::PuckTouch { player, puck, .. } => {
                    self.saved_events.clear();
                    step_events.push_front((player, puck));
                }
                _ => {}
            }
        }

        self.saved_events.push_front((self.game_step, step_events));

        if self.saved_events.len() == 3 {
            let events_five_frame_ago = &self.saved_events[2].1;
            for e in events_five_frame_ago {
                if let Some(skater) = self.world.objects.get_skater(e.0) {
                    if skater.stick_limit == 0.0 || skater.stick_limit > 0.01 {
                        if let Some(puck) = self.world.objects.get_puck_mut(e.1) {
                            puck.body.linear_velocity =
                                limit_vector_length(&puck.body.linear_velocity, 1000.0);
                        }
                    }
                }
            }
        }

        let packets = hqm_parse::get_packets(&self.world.objects.objects);

        behaviour.after_tick(self, &events);

        if self.history_length > 0 {
            let new_replay_tick = ReplayTick {
                game_step: self.game_step,
                packets: packets.clone(),
            };

            self.saved_history.truncate(self.history_length - 1);
            self.saved_history.push_front(new_replay_tick);
        } else {
            self.saved_history.clear();
        }

        self.saved_packets.truncate(192 - 1);
        self.saved_packets.push_front(packets);
        self.packet = self.packet.wrapping_add(1);
        self.saved_pings.truncate(100 - 1);
        self.saved_pings.push_front(Instant::now());

        if self.config.replays_enabled != ReplayEnabled::Off && behaviour.save_replay_data(self) {
            self.write_replay();
        }
    }

    fn remove_inactive_players<B: HQMServerBehaviour>(&mut self, behaviour: &mut B) {
        let inactive_players: smallvec::SmallVec<[_; 8]> = self
            .players
            .iter_mut()
            .filter_map(|(player_index, player)| {
                if let HQMServerPlayerData::NetworkPlayer { data } = &mut player.data {
                    data.inactivity += 1;
                    if data.inactivity > 500 {
                        Some((player_index, player.player_name.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        for (player_index, player_name) in inactive_players {
            behaviour.before_player_exit(self, player_index);
            self.remove_player(player_index, true);
            info!("{} ({}) timed out", player_name, player_index);
            let chat_msg = format!("{} timed out", player_name);
            self.messages.add_server_chat_message(chat_msg);
        }
    }

    async fn tick<B: HQMServerBehaviour>(
        &mut self,
        socket: &UdpSocket,
        behaviour: &mut B,
        write_buf: &mut BytesMut,
    ) {
        if self.player_count() != 0 {
            if !self.has_current_game_been_active {
                self.start_time = Utc::now();
                self.has_current_game_been_active = true;
                behaviour.game_started(self);
                info!("New game {} started", self.game_id);
            }

            let (game_step, forced_view) = tokio::task::block_in_place(|| {
                self.remove_inactive_players(behaviour);

                let has_replay_data = if let Some(replay_element) = self.replay_queue.front_mut() {
                    if let Some(tick) = replay_element.data.pop_front() {
                        Some((replay_element.force_view, tick))
                    } else {
                        self.replay_queue.pop_front();
                        None
                    }
                } else {
                    None
                };

                if let Some((forced_view, tick)) = has_replay_data {
                    let game_step = tick.game_step;
                    let packets = tick.packets;
                    self.saved_packets.truncate(192 - 1);
                    self.saved_packets.push_front(packets);
                    self.saved_pings.truncate(100 - 1);
                    self.saved_pings.push_front(Instant::now());

                    self.packet = self.packet.wrapping_add(1);
                    (game_step, forced_view)
                } else {
                    self.game_step(behaviour);
                    (self.game_step, None)
                }
            });

            for (rec, message) in self.messages.waiting_messages.drain(..) {
                match rec {
                    HQMWaitingMessageReceiver::All => {
                        for (_, player) in self.players.iter_mut() {
                            player.add_message(message.clone());
                        }
                    }
                    HQMWaitingMessageReceiver::Specific(player_index) => {
                        if let Some(player) = self.players.get_mut(player_index) {
                            player.add_message(message);
                        }
                    }
                }
            }

            send_updates(
                self.game_id,
                &self.saved_packets,
                game_step,
                self.values.game_over,
                self.values.red_score,
                self.values.blue_score,
                self.values.time,
                self.values.goal_message_timer,
                self.values.period,
                self.values.rules_state,
                self.packet,
                &self.players.players,
                socket,
                forced_view,
                write_buf,
            )
            .await;

            let game_step = self.game_step;
            while let Some((start_step, end_step, force_view)) = self.requested_replays.pop_front()
            {
                let i_end = game_step.saturating_sub(end_step) as usize;
                let i_start = game_step.saturating_sub(start_step) as usize;
                if i_start <= i_end {
                    continue;
                }
                let data = self
                    .saved_history
                    .range(i_end..=i_start)
                    .rev()
                    .cloned()
                    .collect();
                self.replay_queue
                    .push_back(ReplayElement { data, force_view })
            }
        } else if self.has_current_game_been_active {
            info!("Game {} abandoned", self.game_id);
            self.new_game(behaviour.get_initial_game_values());
            behaviour.game_started(self);
            self.allow_join = true;
        }
    }

    pub fn new_game(&mut self, v: HQMInitialGameValues) {
        self.values = v.values;
        self.world = HQMGameWorld::new(v.puck_slots, v.physics_configuration);
        self.game_id += 1;
        self.messages.clear();

        self.replay_msg_pos = 0;
        self.packet = u32::MAX;
        self.replay_last_packet = u32::MAX;
        self.game_step = u32::MAX;

        self.saved_packets.clear();
        self.saved_pings.clear();
        self.saved_history.clear();
        self.replay_queue.clear();
        self.has_current_game_been_active = false;

        let old_replay_data = std::mem::replace(&mut self.replay_data, BytesMut::new());

        if self.config.replays_enabled == ReplayEnabled::On && !old_replay_data.is_empty() {
            let size = old_replay_data.len();
            let mut replay_data = BytesMut::with_capacity(size + 8);
            replay_data.put_u32_le(0u32);
            replay_data.put_u32_le(size as u32);
            replay_data.put_slice(&old_replay_data);
            let replay_data = replay_data.freeze();
            let time = self.start_time.format("%Y-%m-%dT%H%M%S").to_string();
            let file_name = format!("{}.{}.hrp", self.config.server_name, time);
            let server_name = self.config.server_name.clone();
            match self.config.replay_saving {
                ReplaySaving::File => {
                    tokio::spawn(async move {
                        if tokio::fs::create_dir_all("replays").await.is_err() {
                            return;
                        };
                        let path: PathBuf = ["replays", &file_name].iter().collect();

                        let mut file_handle = match File::create(path).await {
                            Ok(file) => file,
                            Err(e) => {
                                println!("{:?}", e);
                                return;
                            }
                        };

                        let _x = file_handle.write(&replay_data).await;
                        let _x = file_handle.sync_all().await;
                    });
                }
                ReplaySaving::Endpoint { ref url } => {
                    let client = self.reqwest_client.clone();
                    let form = reqwest::multipart::Form::new()
                        .text("time", time)
                        .text("server", server_name)
                        .part(
                            "replay",
                            reqwest::multipart::Part::stream(replay_data).file_name(file_name),
                        );

                    let request = client.post(url).multipart(form);
                    tokio::spawn(async move {
                        let _x = request.send().await;
                    });
                }
            }
        }

        for (player_index, p) in self.players.players.iter_mut().enumerate() {
            let player_index = HQMServerPlayerIndex(player_index);
            if let Some(player) = p {
                if player.reset(player_index) {
                    let update = player.get_update_message(player_index);
                    self.messages.add_global_message(update, true, true);
                } else {
                    let update = HQMMessage::PlayerUpdate {
                        player_name: player.player_name.clone(),
                        object: None,
                        player_index,
                        in_server: false,
                    };
                    self.messages.add_global_message(update, false, false);
                    *p = None;
                }
            }
        }
    }

    pub fn add_replay_to_queue(
        &mut self,
        start_step: u32,
        end_step: u32,
        force_view: Option<HQMServerPlayerIndex>,
    ) {
        if start_step > end_step {
            warn!("start_step must be less than or equal to end_step");
            return;
        }
        self.requested_replays
            .push_back((start_step, end_step, force_view));
    }

    pub fn current_game_id(&self) -> u32 {
        self.game_id
    }

    pub fn replay_data(&self) -> &[u8] {
        self.replay_data.as_ref()
    }

    fn write_replay(&mut self) {
        let replay_messages_to_send = &self.messages.replay_messages[self.replay_msg_pos..];
        let remaining_messages = replay_messages_to_send.len();
        self.replay_data.reserve(
            9 // Header, time, score, period, etc.
            + 8 // Position metadata
            + (32*30) // 32 objects that can be at most 30 bytes each
            + 4 // Message metadata
            + remaining_messages * 66, // Chat message can be up to 66 bytes each
        );
        let mut writer = HQMMessageWriter::new(&mut self.replay_data);

        writer.write_byte_aligned(5);
        writer.write_bits(
            1,
            match self.values.game_over {
                true => 1,
                false => 0,
            },
        );
        writer.write_bits(8, self.values.red_score);
        writer.write_bits(8, self.values.blue_score);
        writer.write_bits(16, self.values.time);

        writer.write_bits(16, self.values.goal_message_timer);
        writer.write_bits(8, self.values.period); // 8.1

        let packets = &self.saved_packets;

        hqm_parse::write_objects(&mut writer, packets, self.packet, self.replay_last_packet);
        self.replay_last_packet = self.packet;

        writer.write_bits(16, remaining_messages as u32);
        writer.write_bits(16, self.replay_msg_pos as u32);

        for message in replay_messages_to_send {
            hqm_parse::write_message(&mut writer, Rc::as_ref(message));
        }
        self.replay_msg_pos = self.messages.replay_messages.len();
        writer.replay_fix();
    }
}

#[derive(Clone, Debug)]
struct ReplayTick {
    game_step: u32,
    packets: [HQMObjectPacket; 32],
}

struct ReplayElement {
    data: VecDeque<ReplayTick>,
    force_view: Option<HQMServerPlayerIndex>,
}

pub async fn run_server<B: HQMServerBehaviour>(
    port: u16,
    public: Option<&str>,
    config: HQMServerConfiguration,
    mut behaviour: B,
) -> std::io::Result<()> {
    let mut player_vec = Vec::with_capacity(64);
    for _ in 0..64 {
        player_vec.push(None);
    }
    let initial_values = behaviour.get_initial_game_values();

    let reqwest_client = reqwest::Client::new();

    let mut server = HQMServer {
        players: HQMServerPlayerList {
            players: player_vec,
        },
        messages: HQMServerMessages::new(),
        ban_list: HashSet::new(),
        allow_join: true,
        values: initial_values.values,
        world: HQMGameWorld::new(
            initial_values.puck_slots,
            initial_values.physics_configuration,
        ),
        is_muted: false,
        config,
        game_id: 1,
        replay_queue: VecDeque::new(),
        requested_replays: VecDeque::new(),
        reqwest_client: reqwest_client.clone(),
        replay_data: BytesMut::with_capacity(64 * 1024 * 1024),
        replay_msg_pos: 0,
        packet: u32::MAX,
        replay_last_packet: u32::MAX,

        saved_packets: VecDeque::with_capacity(192),
        saved_pings: VecDeque::with_capacity(100),
        saved_history: VecDeque::new(),
        has_current_game_been_active: false,
        history_length: 0,
        game_step: u32::MAX,
        start_time: Default::default(),
        saved_events: VecDeque::with_capacity(3),
    };
    info!("Server started");

    behaviour.init(&mut server);

    // Set up timers
    let mut tick_timer = tokio::time::interval(Duration::from_millis(10));
    tick_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let socket = Arc::new(tokio::net::UdpSocket::bind(&addr).await?);
    info!(
        "Server listening at address {:?}",
        socket.local_addr().unwrap()
    );

    async fn get_http_response(
        client: &reqwest::Client,
        address: &str,
    ) -> Result<SocketAddr, Box<dyn Error + Send + Sync>> {
        let response = client.get(address).send().await?.text().await?;

        let split = response.split_ascii_whitespace().collect::<Vec<&str>>();

        let addr = split.get(1).unwrap_or(&"").parse::<IpAddr>()?;
        let port = split.get(2).unwrap_or(&"").parse::<u16>()?;
        Ok(SocketAddr::new(addr, port))
    }

    if let Some(public) = public {
        let socket = socket.clone();
        let reqwest_client = reqwest_client.clone();
        let address = public.to_string();
        tokio::spawn(async move {
            loop {
                let master_server = get_http_response(&reqwest_client, &address).await;
                match master_server {
                    Ok(addr) => {
                        for _ in 0..60 {
                            let msg = b"Hock\x20";
                            let res = socket.send_to(msg, addr).await;
                            if res.is_err() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(e);
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            }
        });
    }
    enum Msg {
        Time,
        Message(SocketAddr, HQMClientToServerMessage),
    }

    let timeout_stream = stream! {
        loop {
            tick_timer.tick().await;
            yield Msg::Time;
        }
    };
    tokio::pin!(timeout_stream);
    let packet_stream = {
        let socket = socket.clone();
        stream! {
            let mut buf = BytesMut::with_capacity(512);
            let codec = HQMMessageCodec;
            loop {
                buf.clear();

                match socket.recv_buf_from(&mut buf).await {
                    Ok((_, addr)) => {
                        if let Ok(data) = codec.parse_message(&buf) {
                            yield Msg::Message(addr, data)
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    };
    tokio::pin!(packet_stream);

    let mut stream = futures::stream_select!(timeout_stream, packet_stream);
    let mut write_buf = BytesMut::with_capacity(4096);
    while let Some(msg) = stream.next().await {
        match msg {
            Msg::Time => server.tick(&socket, &mut behaviour, &mut write_buf).await,
            Msg::Message(addr, data) => {
                server
                    .handle_message(addr, &socket, data, &mut behaviour, &mut write_buf)
                    .await
            }
        }
    }
    Ok(())
}

async fn send_updates(
    game_id: u32,
    packets: &VecDeque<[HQMObjectPacket; 32]>,
    game_step: u32,
    game_over: bool,
    red_score: u32,
    blue_score: u32,
    time: u32,
    goal_message_time: u32,
    period: u32,
    rules_state: HQMRulesState,
    current_packet: u32,
    players: &[Option<HQMServerPlayer>],
    socket: &UdpSocket,
    force_view: Option<HQMServerPlayerIndex>,
    write_buf: &mut BytesMut,
) {
    for player in players.iter() {
        if let Some(player) = player {
            if let HQMServerPlayerData::NetworkPlayer { data } = &player.data {
                write_buf.clear();
                let mut writer = HQMMessageWriter::new(write_buf);

                if data.game_id != game_id {
                    writer.write_bytes_aligned(GAME_HEADER);
                    writer.write_byte_aligned(6);
                    writer.write_u32_aligned(game_id);
                } else {
                    writer.write_bytes_aligned(GAME_HEADER);
                    writer.write_byte_aligned(5);
                    writer.write_u32_aligned(game_id);
                    writer.write_u32_aligned(game_step);
                    writer.write_bits(
                        1,
                        match game_over {
                            true => 1,
                            false => 0,
                        },
                    );
                    writer.write_bits(8, red_score);
                    writer.write_bits(8, blue_score);
                    writer.write_bits(16, time);

                    writer.write_bits(16, goal_message_time);
                    writer.write_bits(8, period);
                    let view = force_view.unwrap_or(data.view_player_index).0 as u32;
                    writer.write_bits(8, view);

                    // if using a non-cryptic version, send ping
                    if data.client_version.has_ping() {
                        writer.write_u32_aligned(data.deltatime);
                    }

                    // if baba's second version or above, send rules
                    if data.client_version.has_rules() {
                        let num = match rules_state {
                            HQMRulesState::Regular {
                                offside_warning,
                                icing_warning,
                            } => {
                                let mut res = 0;
                                if offside_warning {
                                    res |= 1;
                                }
                                if icing_warning {
                                    res |= 2;
                                }
                                res
                            }
                            HQMRulesState::Offside => 4,
                            HQMRulesState::Icing => 8,
                        };
                        writer.write_u32_aligned(num);
                    }

                    write_objects(&mut writer, packets, current_packet, data.known_packet);

                    let (start, remaining_messages) = if data.known_msgpos > data.messages.len() {
                        (data.messages.len(), 0)
                    } else {
                        (
                            data.known_msgpos,
                            min(data.messages.len() - data.known_msgpos, 15),
                        )
                    };

                    writer.write_bits(4, remaining_messages as u32);
                    writer.write_bits(16, start as u32);

                    for message in &data.messages[start..start + remaining_messages] {
                        write_message(&mut writer, Rc::as_ref(message));
                    }
                }

                let slice: &[u8] = &write_buf;
                let _ = socket.send_to(slice, data.addr).await;
            }
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum HQMMuteStatus {
    NotMuted,
    ShadowMuted,
    Muted,
}
pub struct HQMNetworkPlayerData {
    pub addr: SocketAddr,
    pub(crate) client_version: HQMClientVersion,
    inactivity: u32,
    pub(crate) known_packet: u32,
    pub(crate) known_msgpos: usize,
    chat_rep: Option<u8>,
    pub(crate) deltatime: u32,
    last_ping: VecDeque<f32>,
    pub(crate) view_player_index: HQMServerPlayerIndex,
    pub game_id: u32,
    pub(crate) messages: Vec<Rc<HQMMessage>>,
}

pub enum HQMServerPlayerData {
    NetworkPlayer { data: HQMNetworkPlayerData },
}

pub struct HQMServerPlayer {
    pub player_name: Rc<String>,
    pub object: Option<(HQMObjectIndex, HQMTeam)>,
    pub id: Uuid,
    pub data: HQMServerPlayerData,
    pub is_admin: bool,
    pub is_muted: HQMMuteStatus,
    pub hand: HQMSkaterHand,
    pub mass: f32,
    pub input: HQMPlayerInput,
    pub stick_limit: f32,
}

impl HQMServerPlayer {
    pub fn new_network_player(
        player_index: HQMServerPlayerIndex,
        player_name: String,
        addr: SocketAddr,
        global_messages: &[Rc<HQMMessage>],
    ) -> Self {
        HQMServerPlayer {
            player_name: Rc::new(player_name),
            object: None,
            id: Uuid::new_v4(),
            data: HQMServerPlayerData::NetworkPlayer {
                data: HQMNetworkPlayerData {
                    addr,
                    client_version: HQMClientVersion::Vanilla,
                    inactivity: 0,
                    known_packet: u32::MAX,
                    known_msgpos: 0,
                    chat_rep: None,
                    // store latest deltime client sends you to respond with it
                    deltatime: 0,
                    last_ping: VecDeque::new(),
                    view_player_index: player_index,
                    game_id: u32::MAX,
                    messages: global_messages.into_iter().cloned().collect(),
                },
            },
            is_admin: false,
            input: Default::default(),
            is_muted: HQMMuteStatus::NotMuted,
            hand: HQMSkaterHand::Right,
            mass: 1.0,
            stick_limit: 0.00,
        }
    }

    fn reset(&mut self, player_index: HQMServerPlayerIndex) -> bool {
        self.object = None;
        if let HQMServerPlayerData::NetworkPlayer { data } = &mut self.data {
            data.known_msgpos = 0;
            data.known_packet = u32::MAX;
            data.messages.clear();
            data.view_player_index = player_index;
        }
        return true;
    }

    fn get_update_message(&self, player_index: HQMServerPlayerIndex) -> HQMMessage {
        HQMMessage::PlayerUpdate {
            player_name: self.player_name.clone(),
            object: self.object,
            player_index,
            in_server: true,
        }
    }

    fn add_message(&mut self, message: Rc<HQMMessage>) {
        match &mut self.data {
            HQMServerPlayerData::NetworkPlayer {
                data: HQMNetworkPlayerData { messages, .. },
            } => {
                messages.push(message);
            }
            _ => {}
        }
    }

    pub fn addr(&self) -> Option<SocketAddr> {
        match self.data {
            HQMServerPlayerData::NetworkPlayer {
                data: HQMNetworkPlayerData { addr, .. },
            } => Some(addr),
        }
    }

    pub fn ping_data(&self) -> Option<PingData> {
        match self.data {
            HQMServerPlayerData::NetworkPlayer {
                data: HQMNetworkPlayerData { ref last_ping, .. },
            } => {
                let n = last_ping.len() as f32;
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                let mut sum = 0f32;
                for i in last_ping.iter() {
                    min = min.min(*i);
                    max = max.max(*i);
                    sum += *i;
                }
                let avg = sum / n;
                let dev = {
                    let mut s = 0f32;
                    for i in last_ping.iter() {
                        s += (*i - avg).powi(2);
                    }
                    (s / n).sqrt()
                };
                Some(PingData {
                    min,
                    max,
                    avg,
                    deviation: dev,
                })
            }
        }
    }
}

#[derive(Copy, Clone)]
pub struct PingData {
    pub min: f32,
    pub max: f32,
    pub avg: f32,
    pub deviation: f32,
}

#[derive(Debug, Clone)]
pub enum ReplaySaving {
    File,
    Endpoint { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum ReplayEnabled {
    Off,
    On,
    Standby,
}

#[derive(Debug, Clone)]
pub struct HQMServerConfiguration {
    pub welcome: Vec<String>,
    pub password: String,
    pub player_max: usize,

    pub replays_enabled: ReplayEnabled,
    pub replay_saving: ReplaySaving,
    pub server_name: String,
    pub server_service: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HQMInitialGameValues {
    pub values: HQMGameValues,
    pub puck_slots: usize,
    pub physics_configuration: HQMPhysicsConfiguration,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum HQMTeam {
    Red,
    Blue,
}

impl HQMTeam {
    pub(crate) fn get_num(self) -> u32 {
        match self {
            HQMTeam::Red => 0,
            HQMTeam::Blue => 1,
        }
    }

    pub fn get_other_team(self) -> Self {
        match self {
            HQMTeam::Red => HQMTeam::Blue,
            HQMTeam::Blue => HQMTeam::Red,
        }
    }
}

impl Display for HQMTeam {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            HQMTeam::Red => write!(f, "Red"),
            HQMTeam::Blue => write!(f, "Blue"),
        }
    }
}
