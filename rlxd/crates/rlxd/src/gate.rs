//! The app gate: which devices are paired as the gate, and which connections hold it now
//! (handoff 0.10.3 rev 6, 5.1 and 7.2, with the hub's per-device gate of 2026-11-25).
//!
//! - **A credential per device.** `gate.pair` mints a named device's `gate_credential`
//!   and returns it once. rlxd keeps only its SHA-256, in `<home>/rlxd/gate.json`
//!   (0600), with the device's name, its pairing route (`local`, or `ssh` once SSH
//!   pairing lands), when the credential was made and when the device last said hello.
//!   That last time is kept in memory while rlxd runs, and written with the next change
//!   to the devices or when rlxd stops, so a hello never rewrites the file.
//! - **Trust on first use.** While no device is paired, the first `gate.pair` from a peer
//!   that passes the identity check pairs. Re-pairing a device needs that device's
//!   current credential, and replaces it: every connection that used the old one is
//!   dropped. Anything else gets `not_gate` and changes nothing.
//! - **A new device, once one is paired,** needs an approval: its `gate.pair` waits up to
//!   [`PAIR_WAIT`] for a paired device's app (`gate.pair_answer`) or the user on the host
//!   (`rlxd gate pair --request-id`) to approve it. A denial or no answer gets `not_gate`.
//!   A request whose client leaves is withdrawn, and a new device may take the name of
//!   one the user unpaired only when its approver says `force`.
//! - **Unpairing** is the user's, in a terminal (`rlxd gate unpair [<device>]`). It
//!   removes one device, or every device, and drops their connections. Unpairing one
//!   never touches another. With none left, `gate.json` is gone and the next `gate.pair`
//!   is a first pairing again.
//! - **Dev mode** relaxes only the peer identity check; every rule here still holds.
//!
//! **Who approves a new device, and what that proves.** A paired device's app, or a
//! command-line or TUI client on the host: the user running `rlxd gate pair`. Any process
//! of the user's can do what the user's terminal does, so the approval alone does not
//! keep same-user code out. What does is the requester's own peer check, which still
//! applies: on macOS only the signed app can be approved. On Linux and Windows, where
//! that check is best-effort, same-user code could ask and approve. The hub's open
//! question q1 is the same ground for SSH pairing, which is not built here.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, watch};

use crate::log::warning;
use crate::{clock, files, token};

/// The device name a `gate.pair` without one pairs as.
pub const DEFAULT_DEVICE: &str = "default";

/// How long a new device's `gate.pair` waits for an approval.
pub const PAIR_WAIT: Duration = Duration::from_secs(120);

/// The longest device name, in characters.
const DEVICE_MAX: usize = 64;

/// The most pairing requests that wait at once, so no client floods the paired apps.
const PENDING_MAX: usize = 4;

/// `gate.json`'s format version.
const FILE_VERSION: u32 = 1;

/// How a device was paired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// Over the local socket, from a peer that passed the identity check.
    Local,
    /// Inside an authenticated SSH session, as the host's user. Its connections' peer is
    /// sshd, so the credential alone would admit them. SSH pairing isn't built yet, so
    /// such a device is refused (see [`ssh_refused`]).
    Ssh,
}

impl Route {
    /// The route as `gate.json` and the replies write it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
        }
    }
}

/// One paired device, as `gate.json` holds it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Device {
    device: String,
    route: Route,
    /// The SHA-256 of its credential, in hex.
    sha256: String,
    /// When its current credential was minted.
    created: String,
    /// When it last said `hello {role: "app"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    devices: Vec<Device>,
    /// Names of devices the user unpaired while others stayed paired. A new device may
    /// take one only when its approver says `force`, so no request can pass itself off
    /// as a device the user revoked. Forgotten with the file, when the last device goes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    revoked: Vec<String>,
}

/// The gate's state: the paired devices, and the app connections holding the gate now.
pub struct Gate {
    path: PathBuf,
    dev_mode: bool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    devices: Vec<Device>,
    revoked: Vec<String>,
    holders: HashMap<u64, Holder>,
    pending: Vec<Pending>,
    next: u64,
    /// A `last_seen` changed since gate.json was written.
    unsaved: bool,
}

/// An app connection holding the gate.
struct Holder {
    device: String,
    sha256: String,
    kick: watch::Sender<bool>,
}

/// A new device's pairing, waiting for an approval.
struct Pending {
    request_id: String,
    device: String,
    peer_pid: Option<u32>,
    peer_image: Option<String>,
    requested_at: String,
    expires_at: String,
    answer: oneshot::Sender<Answer>,
}

/// How a pairing request was answered, and by whom.
#[derive(Debug)]
pub enum Answer {
    /// Approved by the named device, or the user on the host.
    Approved(String),
    /// Denied likewise.
    Denied(String),
}

/// One app connection's hold on the gate. Dropping it lets go.
pub struct Hold {
    gate: Arc<Gate>,
    id: u64,
    device: String,
    kicked: watch::Receiver<bool>,
}

/// A credential just minted.
#[derive(Debug)]
pub struct Paired {
    /// The new credential, shown this once.
    pub credential: String,
    /// The device it is for.
    pub device: String,
    /// How the device paired.
    pub route: Route,
    /// The app connections dropped because they used the credential it replaced.
    pub dropped: usize,
    /// Whether the caller's own app connection used it, and so is dropped after the
    /// reply.
    pub drop_caller: bool,
    /// The new credential's SHA-256, to revoke it if the reply can't be delivered.
    pub sha256: String,
}

/// Why `gate.pair` did not pair.
#[derive(Debug)]
pub enum Refused {
    /// The caller may not pair this device: `not_gate`.
    NotGate(String),
    /// A new device, while another is paired: it needs an approval.
    NeedsApproval(String),
    /// gate.json could not be written.
    Failed(String),
}

/// Why `gate.pair_answer` answered nothing.
#[derive(Debug)]
pub enum AnswerError {
    /// No request of that id waits.
    NoRequest,
    /// Approving it would give a revoked device's name to a new device, and the
    /// approver did not say `force`.
    Revoked(String),
}

/// What `gate.unpair` could not do.
#[derive(Debug)]
pub enum UnpairError {
    /// No device of that name is paired.
    Unknown(String),
    /// gate.json could not be written or removed.
    Failed(String),
}

/// A waiting pairing request, and where its answer comes.
pub struct Request {
    /// Its id, for `gate.pair_answer`.
    pub request_id: String,
    /// The answer, when one comes.
    pub answer: oneshot::Receiver<Answer>,
}

impl Gate {
    /// The gate as `path` (gate.json) says, in dev mode or not. A missing file is no
    /// device paired. A file that is there but not valid fails the start: rlxd never
    /// reads a broken file as "nothing paired", which would let anyone pair first. A
    /// device with route `ssh` is kept, so the user sees and unpairs it, but refused
    /// (see [`ssh_refused`]), and logged.
    pub fn load(path: &Path, dev_mode: bool) -> Result<Self, String> {
        let (devices, revoked) = read(path)?;
        for device in devices.iter().filter(|d| d.route == Route::Ssh) {
            warning!("gate: {}", ssh_refused(&device.device));
        }
        Ok(Self {
            path: path.to_path_buf(),
            dev_mode,
            state: Mutex::new(State {
                devices,
                revoked,
                ..State::default()
            }),
        })
    }

    /// Whether rlxd runs in gate dev mode.
    pub fn dev_mode(&self) -> bool {
        self.dev_mode
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The device `credential` belongs to, and its route.
    pub fn device_of(&self, credential: &str) -> Option<(String, Route)> {
        let state = self.state();
        let index = find(&state.devices, credential)?;
        let device = &state.devices[index];
        Some((device.device.clone(), device.route))
    }

    /// Makes a connection presenting `credential` a holder of the gate, and notes when
    /// its device was last seen. `None` when the credential is no longer valid, as when
    /// it was replaced or unpaired while the connection said hello.
    pub fn attach(self: &Arc<Self>, credential: &str) -> Option<Hold> {
        let mut state = self.state();
        let index = find(&state.devices, credential)?;
        state.devices[index].last_seen = Some(clock::now());
        state.unsaved = true;
        let device = state.devices[index].device.clone();
        let sha256 = state.devices[index].sha256.clone();
        state.next += 1;
        let id = state.next;
        let (kick, kicked) = watch::channel(false);
        state.holders.insert(
            id,
            Holder {
                device: device.clone(),
                sha256,
                kick,
            },
        );
        Some(Hold {
            gate: Arc::clone(self),
            id,
            device,
            kicked,
        })
    }

    /// The app connections holding the gate now: `status`'s `clients.app`.
    pub fn app_connections(&self) -> u32 {
        u32::try_from(self.state().holders.len()).unwrap_or(u32::MAX)
    }

    /// `gate.pair`, for a caller whose peer already passed the identity check (or in dev
    /// mode). `caller` is the caller's own hold, if its connection holds the gate.
    pub fn pair(
        &self,
        device: Option<&str>,
        credential: Option<&str>,
        caller: Option<u64>,
    ) -> Result<Paired, Refused> {
        let mut state = self.state();
        if let Some(credential) = credential {
            let Some(index) = find(&state.devices, credential) else {
                return Err(Refused::NotGate(
                    "that gate_credential is not one this rlxd paired: it is wrong, was \
                     replaced, or its device was unpaired"
                        .to_owned(),
                ));
            };
            let paired = state.devices[index].device.clone();
            if state.devices[index].route == Route::Ssh {
                return Err(Refused::NotGate(ssh_refused(&paired)));
            }
            if let Some(device) = device
                && device != paired
            {
                return Err(Refused::NotGate(format!(
                    "that gate_credential is device {paired:?}'s, not {device:?}'s"
                )));
            }
            let (new, sha256) = mint().map_err(Refused::Failed)?;
            let mut devices = state.devices.clone();
            let old = std::mem::replace(&mut devices[index].sha256, sha256);
            devices[index].created = clock::now();
            let sha256 = devices[index].sha256.clone();
            self.save(&devices, &state.revoked)
                .map_err(|e| Refused::Failed(format!("cannot write gate.json: {e}")))?;
            let route = devices[index].route;
            state.devices = devices;
            let drop_caller = caller.is_some_and(|id| {
                state
                    .holders
                    .get(&id)
                    .is_some_and(|holder| holder.sha256 == old)
            });
            let dropped = kick(&mut state, |holder| holder.sha256 == old, caller);
            if drop_caller {
                state.holders.remove(&caller.unwrap_or_default());
            }
            return Ok(Paired {
                credential: new,
                device: paired,
                route,
                dropped: dropped + usize::from(drop_caller),
                drop_caller,
                sha256,
            });
        }
        let device = device.unwrap_or(DEFAULT_DEVICE);
        if state.devices.iter().any(|d| d.device == device) {
            return Err(Refused::NotGate(format!(
                "device {device:?} is paired already: re-pairing it needs its current \
                 gate_credential"
            )));
        }
        if !state.devices.is_empty() {
            return Err(Refused::NeedsApproval(device.to_owned()));
        }
        self.add(&mut state, device, Route::Local)
    }

    /// Pairs `device`, a new device whose request was just approved, unless a device of
    /// that name was paired meanwhile.
    pub fn pair_approved(&self, device: &str) -> Result<Paired, Refused> {
        let mut state = self.state();
        if state.devices.iter().any(|d| d.device == device) {
            return Err(Refused::NotGate(format!(
                "device {device:?} was paired by another request meanwhile"
            )));
        }
        self.add(&mut state, device, Route::Local)
    }

    fn add(&self, state: &mut State, device: &str, route: Route) -> Result<Paired, Refused> {
        let (credential, sha256) = mint().map_err(Refused::Failed)?;
        let mut devices = state.devices.clone();
        devices.push(Device {
            device: device.to_owned(),
            route,
            sha256: sha256.clone(),
            created: clock::now(),
            last_seen: None,
        });
        // An approver who said `force` let the new device take a revoked name.
        let revoked: Vec<String> = state
            .revoked
            .iter()
            .filter(|name| *name != device)
            .cloned()
            .collect();
        self.save(&devices, &revoked)
            .map_err(|e| Refused::Failed(format!("cannot write gate.json: {e}")))?;
        state.devices = devices;
        state.revoked = revoked;
        Ok(Paired {
            credential,
            device: device.to_owned(),
            route,
            dropped: 0,
            drop_caller: false,
            sha256,
        })
    }

    /// Unpairs `device` if its credential is still the one whose SHA-256 is `sha256`: a
    /// credential minted for a reply that never reached its client, which nobody holds.
    /// So no device is ever left paired with a credential nobody received.
    pub fn revoke_undelivered(&self, device: &str, sha256: &str) {
        let mut state = self.state();
        let Some(index) = state
            .devices
            .iter()
            .position(|d| d.device == device && d.sha256 == sha256)
        else {
            return;
        };
        let mut devices = state.devices.clone();
        devices.remove(index);
        match self.save(&devices, &state.revoked) {
            Ok(()) => {
                state.devices = devices;
                let dropped = kick(&mut state, |holder| holder.sha256 == sha256, None);
                warning!(
                    "gate: device {device:?}'s new credential never reached its client, so \
                     the device is unpaired ({dropped} app connection(s) dropped)"
                );
            }
            Err(e) => warning!(
                "gate: device {device:?}'s new credential never reached its client, and \
                 gate.json could not be written to unpair it: {e}"
            ),
        }
    }

    /// Makes a pairing request for the new `device`, from the peer `peer_pid` running
    /// `peer_image`, which waits for an answer. One request per device name waits at a
    /// time.
    pub fn request(
        &self,
        device: &str,
        peer_pid: Option<u32>,
        peer_image: Option<String>,
    ) -> Result<Request, Refused> {
        let mut state = self.state();
        if state.pending.iter().any(|p| p.device == device) {
            return Err(Refused::NotGate(format!(
                "a pairing request for device {device:?} is already waiting"
            )));
        }
        if state.pending.len() >= PENDING_MAX {
            return Err(Refused::NotGate(format!(
                "{PENDING_MAX} pairing requests already wait for an answer"
            )));
        }
        let request_id = token::random_hex()
            .map_err(|e| Refused::Failed(e.to_string()))?
            .chars()
            .take(16)
            .collect::<String>();
        let (answer, answered) = oneshot::channel();
        let now = SystemTime::now();
        state.pending.push(Pending {
            request_id: request_id.clone(),
            device: device.to_owned(),
            peer_pid,
            peer_image,
            requested_at: clock::format(now),
            expires_at: clock::format(now + PAIR_WAIT),
            answer,
        });
        Ok(Request {
            request_id,
            answer: answered,
        })
    }

    /// Drops a request that is no longer waited on.
    pub fn withdraw(&self, request_id: &str) {
        self.state().pending.retain(|p| p.request_id != request_id);
    }

    /// The requests waiting for an answer: each one's id, device name, route, the peer
    /// that asked (pid and binary), and whether the name is one the user revoked.
    pub fn requests(&self) -> Vec<Value> {
        let state = self.state();
        state
            .pending
            .iter()
            .map(|p| {
                json!({
                    "request_id": p.request_id,
                    "device": p.device,
                    "route": Route::Local.name(),
                    "peer": {"pid": p.peer_pid, "image": p.peer_image},
                    "revoked": state.revoked.contains(&p.device),
                    "requested_at": p.requested_at,
                    "expires_at": p.expires_at,
                })
            })
            .collect()
    }

    /// Answers the waiting request `request_id`. Approving one for a name the user
    /// revoked needs `force`. Returns its device.
    pub fn answer(
        &self,
        request_id: &str,
        approve: bool,
        force: bool,
        by: &str,
    ) -> Result<String, AnswerError> {
        let mut state = self.state();
        let index = state
            .pending
            .iter()
            .position(|p| p.request_id == request_id)
            .ok_or(AnswerError::NoRequest)?;
        let device = &state.pending[index].device;
        if approve && !force && state.revoked.contains(device) {
            return Err(AnswerError::Revoked(device.clone()));
        }
        let pending = state.pending.remove(index);
        let answer = if approve {
            Answer::Approved(by.to_owned())
        } else {
            Answer::Denied(by.to_owned())
        };
        // A requester that gave up has dropped its receiver: nothing to tell.
        let _ = pending.answer.send(answer);
        Ok(pending.device)
    }

    /// The paired devices, each with its open app connections.
    pub fn devices(&self) -> Vec<Value> {
        let state = self.state();
        state
            .devices
            .iter()
            .map(|d| {
                let connections = state
                    .holders
                    .values()
                    .filter(|h| h.device == d.device)
                    .count();
                json!({
                    "device": d.device,
                    "route": d.route.name(),
                    "created": d.created,
                    "last_seen": d.last_seen,
                    "connections": connections,
                })
            })
            .collect()
    }

    /// Unpairs `device`, or every device, and drops their app connections. Returns the
    /// devices unpaired and the connections dropped.
    pub fn unpair(&self, device: Option<&str>) -> Result<(Vec<String>, usize), UnpairError> {
        let mut state = self.state();
        let (kept, gone): (Vec<Device>, Vec<Device>) = match device {
            Some(name) => {
                if !state.devices.iter().any(|d| d.device == name) {
                    return Err(UnpairError::Unknown(name.to_owned()));
                }
                state
                    .devices
                    .iter()
                    .cloned()
                    .partition(|d| d.device != name)
            }
            None => (Vec::new(), state.devices.clone()),
        };
        let names: Vec<String> = gone.into_iter().map(|d| d.device).collect();
        // While other devices stay paired, a revoked name stays taken (see `Stored`).
        let mut revoked = Vec::new();
        if !kept.is_empty() {
            revoked.clone_from(&state.revoked);
            for name in &names {
                if !revoked.contains(name) {
                    revoked.push(name.clone());
                }
            }
        }
        self.save(&kept, &revoked)
            .map_err(|e| UnpairError::Failed(format!("cannot write gate.json: {e}")))?;
        state.devices = kept;
        state.revoked = revoked;
        let dropped = match device {
            Some(name) => kick(&mut state, |holder| holder.device == name, None),
            None => kick(&mut state, |_| true, None),
        };
        Ok((names, dropped))
    }

    /// Writes when each device was last seen, if that changed: when rlxd stops.
    pub fn flush(&self) {
        let mut state = self.state();
        if !state.unsaved || state.devices.is_empty() {
            return;
        }
        match self.save(&state.devices, &state.revoked) {
            Ok(()) => state.unsaved = false,
            Err(e) => warning!("gate: cannot write when each device was last seen: {e}"),
        }
    }

    /// Writes `devices` and `revoked` to gate.json, or removes it when no device is left.
    fn save(&self, devices: &[Device], revoked: &[String]) -> io::Result<()> {
        if devices.is_empty() {
            return files::remove(&self.path);
        }
        let stored = Stored {
            version: FILE_VERSION,
            devices: devices.to_vec(),
            revoked: revoked.to_vec(),
        };
        let text = serde_json::to_string_pretty(&stored).map_err(io::Error::other)?;
        files::write_private(&self.path, &text)
    }

    fn detach(&self, id: u64) {
        self.state().holders.remove(&id);
    }
}

impl Hold {
    /// This hold's id, unique while rlxd runs.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The device this connection is.
    pub fn device(&self) -> &str {
        &self.device
    }

    /// Whether the connection was dropped from the gate: its credential was replaced, or
    /// its device unpaired.
    pub fn is_kicked(&self) -> bool {
        *self.kicked.borrow()
    }

    /// Resolves once the connection is dropped from the gate.
    pub async fn kicked(&self) {
        let mut kicked = self.kicked.clone();
        if kicked.wait_for(|kicked| *kicked).await.is_err() {
            // The sender went with its holder entry: this hold was dropped from the
            // gate without a kick, which only `pair` does, for the caller.
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.gate.detach(self.id);
    }
}

/// Removes every holder `drop` picks, except `keep`, and tells each it was dropped.
/// Returns how many.
fn kick(state: &mut State, drop: impl Fn(&Holder) -> bool, keep: Option<u64>) -> usize {
    let ids: Vec<u64> = state
        .holders
        .iter()
        .filter(|(id, holder)| Some(**id) != keep && drop(holder))
        .map(|(id, _)| *id)
        .collect();
    for id in &ids {
        if let Some(holder) = state.holders.remove(id) {
            holder.kick.send_replace(true);
        }
    }
    ids.len()
}

/// The index of the device whose credential is `credential`.
fn find(devices: &[Device], credential: &str) -> Option<usize> {
    let sha256 = hash(credential);
    devices
        .iter()
        .position(|d| token::matches(&d.sha256, &sha256))
}

/// A new credential, and its SHA-256.
fn mint() -> Result<(String, String), String> {
    let credential = token::random_hex().map_err(|e| e.to_string())?;
    let sha256 = hash(&credential);
    Ok((credential, sha256))
}

/// The SHA-256 of `credential`, in hex.
fn hash(credential: &str) -> String {
    Sha256::digest(credential.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The devices and revoked names gate.json at `path` holds: none when it is missing.
fn read(path: &Path) -> Result<(Vec<Device>, Vec<String>), String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let broken = |why: String| {
        format!(
            "{} is not valid ({why}); `rlxd gate unpair` removes it, and the app then pairs \
             again",
            path.display()
        )
    };
    let stored: Stored = serde_json::from_str(&text).map_err(|e| broken(e.to_string()))?;
    if stored.version != FILE_VERSION {
        return Err(broken(format!("version {}", stored.version)));
    }
    Ok((stored.devices, stored.revoked))
}

/// Why a device with route `ssh` is refused. SSH pairing isn't built yet (hub q1), so
/// this rlxd never minted an ssh-route credential: such an entry in gate.json was written
/// by something else, and admitting it on its credential alone, as an SSH device would
/// be, would let an unsigned client hold the gate. It fails closed until SSH pairing
/// lands.
pub fn ssh_refused(device: &str) -> String {
    format!(
        "device {device:?} has route ssh, which this rlxd refuses: SSH pairing is not \
         available yet, so rlxd never paired it (`rlxd gate unpair {device:?}` removes it)"
    )
}

/// Whether `name` may name a device: 1 to 64 characters, none of them control
/// characters.
pub fn valid_device(name: &str) -> Result<(), String> {
    let count = name.chars().count();
    if count == 0 || count > DEVICE_MAX {
        return Err(format!(
            "a device name has 1 to {DEVICE_MAX} characters, not {count}"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("a device name has no control characters".to_owned());
    }
    Ok(())
}

/// Unpairs with rlxd stopped: edits gate.json at `path` directly. The caller holds
/// rlxd's lock, so no rlxd starts meanwhile.
pub fn unpair_offline(path: &Path, device: Option<&str>) -> Result<Vec<String>, UnpairError> {
    let (devices, revoked) = match (read(path), device) {
        (Ok(read), _) => read,
        // A broken file can still be removed whole.
        (Err(_), None) => {
            files::remove(path).map_err(|e| UnpairError::Failed(e.to_string()))?;
            return Ok(Vec::new());
        }
        (Err(e), Some(_)) => return Err(UnpairError::Failed(e)),
    };
    let gate = Gate {
        path: path.to_path_buf(),
        dev_mode: false,
        state: Mutex::new(State {
            devices,
            revoked,
            ..State::default()
        }),
    };
    gate.unpair(device).map(|(names, _)| names)
}

/// The devices gate.json at `path` holds, as `gate.devices` lists them, with rlxd
/// stopped: no connections.
pub fn devices_offline(path: &Path) -> Result<Vec<Value>, String> {
    let gate = Gate::load(path, false)?;
    Ok(gate.devices())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gate in a fresh folder, removed when the test ends.
    struct Fixture {
        gate: Arc<Gate>,
        dir: PathBuf,
    }

    impl std::ops::Deref for Fixture {
        type Target = Arc<Gate>;
        fn deref(&self) -> &Arc<Gate> {
            &self.gate
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn gate(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("rlxd-gate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let gate = Arc::new(Gate::load(&dir.join("gate.json"), false).unwrap());
        Fixture { gate, dir }
    }

    #[test]
    fn the_first_pairing_is_trust_on_first_use_and_a_second_is_not_gate() {
        let gate = gate("first");
        let first = gate.pair(None, None, None).unwrap();
        assert_eq!(first.device, DEFAULT_DEVICE);
        assert_eq!(first.credential.len(), 64);
        let stored = fs::read_to_string(&gate.path).unwrap();
        assert!(!stored.contains(&first.credential), "only the hash is kept");
        assert!(stored.contains(&hash(&first.credential)));
        assert!(matches!(
            gate.pair(None, None, None),
            Err(Refused::NotGate(_))
        ));
        assert!(matches!(
            gate.pair(None, Some("wrong"), None),
            Err(Refused::NotGate(_))
        ));
        assert!(matches!(
            gate.pair(Some("laptop"), None, None),
            Err(Refused::NeedsApproval(_))
        ));
    }

    #[test]
    fn re_pairing_replaces_the_credential_and_drops_its_connections_only() {
        let gate = gate("repair");
        let a = gate.pair(None, None, None).unwrap();
        let request = gate.request("b", Some(1), None).unwrap();
        gate.answer(&request.request_id, true, false, "test")
            .unwrap();
        let b = gate.pair_approved("b").unwrap();
        let hold_a = gate.attach(&a.credential).unwrap();
        let hold_b = gate.attach(&b.credential).unwrap();
        assert_eq!(gate.app_connections(), 2);
        let again = gate.pair(None, Some(&a.credential), None).unwrap();
        assert_eq!(again.device, DEFAULT_DEVICE);
        assert_eq!(again.dropped, 1);
        assert!(hold_a.is_kicked() && !hold_b.is_kicked());
        assert_eq!(gate.app_connections(), 1);
        assert!(
            gate.attach(&a.credential).is_none(),
            "the old one is refused"
        );
        assert!(gate.attach(&again.credential).is_some());
        assert!(matches!(
            gate.pair(Some("b"), Some(&again.credential), None),
            Err(Refused::NotGate(_))
        ));
    }

    #[test]
    fn the_callers_own_connection_is_dropped_after_its_reply() {
        let gate = gate("caller");
        let a = gate.pair(None, None, None).unwrap();
        let hold = gate.attach(&a.credential).unwrap();
        let again = gate
            .pair(None, Some(&a.credential), Some(hold.id()))
            .unwrap();
        assert!(again.drop_caller);
        assert!(!hold.is_kicked(), "the caller is told after its reply");
        assert_eq!(gate.app_connections(), 0);
    }

    #[test]
    fn unpairing_one_device_leaves_the_other_and_the_last_removes_the_file() {
        let gate = gate("unpair");
        let a = gate.pair(None, None, None).unwrap();
        let request = gate.request("b", Some(1), None).unwrap();
        gate.answer(&request.request_id, true, false, "test")
            .unwrap();
        let b = gate.pair_approved("b").unwrap();
        let hold_a = gate.attach(&a.credential).unwrap();
        let hold_b = gate.attach(&b.credential).unwrap();
        let (names, dropped) = gate.unpair(Some("b")).unwrap();
        assert_eq!((names, dropped), (vec!["b".to_owned()], 1));
        assert!(hold_b.is_kicked() && !hold_a.is_kicked());
        assert!(gate.attach(&a.credential).is_some());
        assert!(matches!(
            gate.unpair(Some("b")),
            Err(UnpairError::Unknown(_))
        ));
        gate.unpair(None).unwrap();
        assert!(hold_a.is_kicked());
        assert!(!gate.path.exists());
        assert!(gate.pair(None, None, None).is_ok(), "a first pairing again");
    }

    #[test]
    fn a_broken_gate_json_fails_the_load_and_unpair_removes_it() {
        let gate = gate("broken");
        fs::write(&gate.path, "{not json").unwrap();
        assert!(Gate::load(&gate.path, false).is_err());
        assert!(unpair_offline(&gate.path, Some("x")).is_err());
        unpair_offline(&gate.path, None).unwrap();
        assert!(!gate.path.exists());
    }

    #[test]
    fn requests_wait_one_per_device_and_a_few_at_once() {
        let gate = gate("pending");
        gate.pair(None, None, None).unwrap();
        let first = gate.request("a", Some(1), None).unwrap();
        assert!(matches!(
            gate.request("a", Some(2), None),
            Err(Refused::NotGate(_))
        ));
        let _rest: Vec<Request> = (1..PENDING_MAX)
            .map(|n| gate.request(&format!("d{n}"), Some(3), None).unwrap())
            .collect();
        assert!(matches!(
            gate.request("one more", Some(4), None),
            Err(Refused::NotGate(_))
        ));
        gate.withdraw(&first.request_id);
        assert!(gate.request("one more", Some(4), None).is_ok());
        assert!(matches!(
            gate.answer("no such id", true, false, "test"),
            Err(AnswerError::NoRequest)
        ));
    }

    #[test]
    fn a_revoked_name_needs_force_while_other_devices_stay_paired() {
        let gate = gate("revoked");
        let a = gate.pair(None, None, None).unwrap();
        let request = gate.request("laptop", None, None).unwrap();
        gate.answer(&request.request_id, true, false, "test")
            .unwrap();
        gate.pair_approved("laptop").unwrap();
        gate.unpair(Some("laptop")).unwrap();
        let again = gate.request("laptop", Some(7), Some("/x".into())).unwrap();
        assert_eq!(gate.requests()[0]["revoked"], true);
        assert_eq!(gate.requests()[0]["peer"]["pid"], 7);
        assert!(matches!(
            gate.answer(&again.request_id, true, false, "test"),
            Err(AnswerError::Revoked(_))
        ));
        assert_eq!(gate.requests().len(), 1, "it still waits");
        gate.answer(&again.request_id, true, true, "test").unwrap();
        gate.pair_approved("laptop").unwrap();
        let reloaded = Gate::load(&gate.path, false).unwrap();
        assert!(
            reloaded.state().revoked.is_empty(),
            "force took the name back"
        );
        // Unpaired again while "default" stays, the name is recorded; unpairing every
        // device forgets it with gate.json.
        gate.unpair(Some("laptop")).unwrap();
        assert_eq!(
            Gate::load(&gate.path, false).unwrap().state().revoked,
            ["laptop"]
        );
        assert!(gate.attach(&a.credential).is_some(), "default is untouched");
        gate.unpair(None).unwrap();
        assert!(!gate.path.exists());
    }

    #[test]
    fn an_undelivered_credential_unpairs_its_device() {
        let gate = gate("undelivered");
        let a = gate.pair(None, None, None).unwrap();
        let again = gate.pair(None, Some(&a.credential), None).unwrap();
        gate.revoke_undelivered(&again.device, &a.sha256);
        assert_eq!(
            gate.devices().len(),
            1,
            "an older credential's hash changes nothing"
        );
        gate.revoke_undelivered(&again.device, &again.sha256);
        assert!(gate.devices().is_empty());
        assert!(!gate.path.exists());
    }

    #[test]
    fn a_device_with_route_ssh_is_kept_but_never_re_paired() {
        let gate = gate("ssh");
        let credential = "planted";
        let planted = format!(
            "{{\"version\":1,\"devices\":[{{\"device\":\"x\",\"route\":\"ssh\",\
             \"sha256\":\"{}\",\"created\":\"2026-11-26T00:00:00.000Z\"}}]}}",
            hash(credential)
        );
        fs::write(&gate.path, planted).unwrap();
        let loaded = Gate::load(&gate.path, false).unwrap();
        assert_eq!(loaded.device_of(credential).unwrap().1, Route::Ssh);
        assert!(matches!(
            loaded.pair(None, Some(credential), None),
            Err(Refused::NotGate(_))
        ));
        assert_eq!(loaded.devices().len(), 1);
    }

    #[test]
    fn a_device_name_is_short_and_printable() {
        assert!(valid_device("Anmol's MacBook").is_ok());
        assert!(valid_device("").is_err());
        assert!(valid_device(&"x".repeat(65)).is_err());
        assert!(valid_device("a\nb").is_err());
    }
}
