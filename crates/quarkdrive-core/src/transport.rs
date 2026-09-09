//! Talking to a Quarkdrive server.
//!
//! The sync engine only knows about five operations, which keeps it testable
//! and lets the Android client swap in its own transport later:
//!
//! * read the vault's current head
//! * fetch an object by id
//! * ask which of a batch of objects already exist
//! * upload an object
//! * commit a new snapshot, conditional on the head being where we saw it
//!
//! Only objects are transferred, never "files": chunking and tree building are
//! entirely client-side, so the server does no content-specific work for
//! desktop clients.
//!
//! [`MemoryVault`] implements the same trait against a hash map. The tests
//! drive the real protocol against it — optimistic concurrency included —
//! without needing a listening socket.

use crate::hash::ObjectId;
use crate::tree::Snapshot;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Outcome of a commit attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Accepted; carries the id of the stored snapshot.
    Accepted(ObjectId),
    /// Rejected because the head moved on between our read and our write.
    /// The caller re-merges and retries.
    Conflict { current: Option<ObjectId> },
}

/// Everything a sync client needs from the other side.
///
/// Methods take `&mut self` because uploads and commits mutate remote state;
/// keeping one receiver avoids forcing interior mutability on implementors.
pub trait Transport {
    /// Current head snapshot and its id, or `None` for an empty vault.
    fn head(&mut self) -> Result<Option<(ObjectId, Snapshot)>>;

    /// Raw stored bytes for an object, or `None` if the server lacks it.
    fn get_object(&mut self, id: &ObjectId) -> Result<Option<Vec<u8>>>;

    /// Which of these objects does the server already hold?
    ///
    /// Batching matters: a vault with a million objects would otherwise cost a
    /// million round trips.
    fn has_objects(&mut self, ids: &[ObjectId]) -> Result<Vec<bool>>;

    /// Upload an object. Returns whether it was newly stored.
    fn put_object(&mut self, id: &ObjectId, encoded: &[u8]) -> Result<bool>;

    /// Commit `snapshot`, but only if the head is still `expected_parent`.
    fn commit(
        &mut self,
        expected_parent: Option<ObjectId>,
        snapshot: &Snapshot,
    ) -> Result<CommitOutcome>;
}

// ------------------------------------------------------------------- HTTP

#[derive(Serialize)]
struct HaveRequest<'a> {
    ids: &'a [ObjectId],
}

#[derive(Deserialize)]
struct HaveResponse {
    have: Vec<bool>,
}

#[derive(Deserialize)]
struct HeadResponse {
    snapshot: Option<ObjectId>,
}

#[derive(Serialize)]
struct CommitRequest<'a> {
    parent: Option<ObjectId>,
    snapshot: &'a Snapshot,
}

#[derive(Deserialize)]
struct CommitResponse {
    snapshot: ObjectId,
}

#[derive(Deserialize)]
struct ConflictResponse {
    current: Option<ObjectId>,
}

#[derive(Deserialize)]
struct PutResponse {
    created: bool,
}

/// HTTP transport for the server's `/api/v1` endpoints.
pub struct HttpTransport {
    base: String,
    vault: String,
    token: String,
    agent: ureq::Agent,
}

impl HttpTransport {
    pub fn new(base: &str, vault: &str, token: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            // A connect timeout catches dead hosts. Reads are left unbounded
            // because one object upload can legitimately take minutes.
            .timeout_connect(Duration::from_secs(20))
            .build();
        HttpTransport {
            base: base.trim_end_matches('/').to_string(),
            vault: vault.to_string(),
            token: token.to_string(),
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/vaults/{}/{}", self.base, self.vault, path)
    }

    fn authed(&self, req: ureq::Request) -> ureq::Request {
        req.set("Authorization", &format!("Bearer {}", self.token))
    }
}

/// ureq gates `Response::into_json` behind a non-default feature, so bodies
/// are read as text and parsed with serde directly.
fn json_resp<T: serde::de::DeserializeOwned>(resp: ureq::Response) -> Result<T> {
    let body = resp.into_string().context("reading response body")?;
    Ok(serde_json::from_str(&body)?)
}

/// Read a binary response body.
fn bytes_resp(resp: ureq::Response) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    resp.into_reader()
        .read_to_end(&mut out)
        .context("reading response body")?;
    Ok(out)
}

fn transport_err(err: ureq::Error, what: &str) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow!("{}: server returned {}: {}", what, code, body)
        }
        ureq::Error::Transport(t) => anyhow!("{}: {}", what, t),
    }
}

impl Transport for HttpTransport {
    fn head(&mut self) -> Result<Option<(ObjectId, Snapshot)>> {
        let resp = self
            .authed(self.agent.get(&self.url("head")))
            .call()
            .map_err(|e| transport_err(e, "reading head"))?;
        let head: HeadResponse = json_resp(resp)?;
        match head.snapshot {
            None => Ok(None),
            Some(id) => {
                let resp = self
                    .authed(self.agent.get(&self.url(&format!("snapshots/{id}"))))
                    .call();
                match resp {
                    Ok(r) => Ok(Some((id, json_resp(r)?))),
                    Err(ureq::Error::Status(404, _)) => Err(anyhow!(
                        "server reports head {id} but has no such snapshot"
                    )),
                    Err(e) => Err(transport_err(e, "fetching snapshot")),
                }
            }
        }
    }

    fn get_object(&mut self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let resp = self
            .authed(self.agent.get(&self.url(&format!("objects/{id}"))))
            .call();
        match resp {
            Ok(r) => Ok(Some(bytes_resp(r)?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(transport_err(e, "fetching object")),
        }
    }

    fn has_objects(&mut self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::to_vec(&HaveRequest { ids })?;
        let resp = self
            .authed(self.agent.post(&self.url("objects/have")))
            .set("Content-Type", "application/json")
            .send_bytes(&body)
            .map_err(|e| transport_err(e, "querying objects"))?;
        let parsed: HaveResponse = json_resp(resp)?;
        if parsed.have.len() != ids.len() {
            return Err(anyhow!(
                "server answered {} of {} ids",
                parsed.have.len(),
                ids.len()
            ));
        }
        Ok(parsed.have)
    }

    fn put_object(&mut self, id: &ObjectId, encoded: &[u8]) -> Result<bool> {
        let resp = self
            .authed(self.agent.put(&self.url(&format!("objects/{id}"))))
            .set("Content-Type", "application/octet-stream")
            .send_bytes(encoded)
            .map_err(|e| transport_err(e, "uploading object"))?;
        let parsed: PutResponse = json_resp(resp)?;
        Ok(parsed.created)
    }

    fn commit(
        &mut self,
        expected_parent: Option<ObjectId>,
        snapshot: &Snapshot,
    ) -> Result<CommitOutcome> {
        let body = serde_json::to_vec(&CommitRequest {
            parent: expected_parent,
            snapshot,
        })?;
        let result = self
            .authed(self.agent.post(&self.url("commit")))
            .set("Content-Type", "application/json")
            .send_bytes(&body);
        match result {
            Ok(resp) => {
                let parsed: CommitResponse = json_resp(resp)?;
                Ok(CommitOutcome::Accepted(parsed.snapshot))
            }
            Err(ureq::Error::Status(409, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let parsed: ConflictResponse =
                    serde_json::from_str(&body).unwrap_or(ConflictResponse { current: None });
                Ok(CommitOutcome::Conflict {
                    current: parsed.current,
                })
            }
            Err(e) => Err(transport_err(e, "committing snapshot")),
        }
    }
}

// ----------------------------------------------------------------- memory

/// A complete in-memory vault: objects, snapshots and a mutable head.
///
/// Used by the sync tests, and a compact executable description of what the
/// server must do.
#[derive(Debug, Default)]
pub struct MemoryVault {
    pub objects: HashMap<ObjectId, Vec<u8>>,
    pub snapshots: HashMap<ObjectId, Snapshot>,
    pub head: Option<ObjectId>,
    pub commits: usize,
    pub rejected_commits: usize,
    pub uploaded_objects: usize,
    pub uploaded_bytes: usize,
    pub served_objects: usize,
}

impl MemoryVault {
    pub fn new() -> Self {
        MemoryVault::default()
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub fn stored_bytes(&self) -> usize {
        self.objects.values().map(|v| v.len()).sum()
    }

    /// Move the head behind the client's back, as another device committing
    /// would. Used to exercise the retry path.
    pub fn simulate_other_device_commit(&mut self, snapshot: &Snapshot) -> ObjectId {
        let id = ObjectId::hash(&serde_json::to_vec(snapshot).unwrap());
        self.snapshots.insert(id, snapshot.clone());
        self.head = Some(id);
        id
    }
}

impl Transport for MemoryVault {
    fn head(&mut self) -> Result<Option<(ObjectId, Snapshot)>> {
        match self.head {
            None => Ok(None),
            Some(id) => {
                let snap = self
                    .snapshots
                    .get(&id)
                    .ok_or_else(|| anyhow!("head {id} has no snapshot"))?
                    .clone();
                Ok(Some((id, snap)))
            }
        }
    }

    fn get_object(&mut self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.served_objects += 1;
        Ok(self.objects.get(id).cloned())
    }

    fn has_objects(&mut self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        Ok(ids.iter().map(|id| self.objects.contains_key(id)).collect())
    }

    fn put_object(&mut self, id: &ObjectId, encoded: &[u8]) -> Result<bool> {
        if self.objects.contains_key(id) {
            return Ok(false);
        }
        self.objects.insert(*id, encoded.to_vec());
        self.uploaded_objects += 1;
        self.uploaded_bytes += encoded.len();
        Ok(true)
    }

    fn commit(
        &mut self,
        expected_parent: Option<ObjectId>,
        snapshot: &Snapshot,
    ) -> Result<CommitOutcome> {
        if self.head != expected_parent {
            self.rejected_commits += 1;
            return Ok(CommitOutcome::Conflict { current: self.head });
        }
        let id = ObjectId::hash(&serde_json::to_vec(snapshot)?);
        self.snapshots.insert(id, snapshot.clone());
        self.head = Some(id);
        self.commits += 1;
        Ok(CommitOutcome::Accepted(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(root: ObjectId) -> Snapshot {
        Snapshot {
            root,
            device: "test".into(),
            host: "test".into(),
            time: 1,
            parent: None,
            files: 0,
            dirs: 1,
            bytes: 0,
        }
    }

    #[test]
    fn empty_vault_has_no_head() {
        let mut v = MemoryVault::new();
        assert!(v.head().unwrap().is_none());
    }

    /// The guard that makes concurrent syncing safe.
    #[test]
    fn commit_rejects_stale_parent() {
        let mut v = MemoryVault::new();
        let root = ObjectId::hash(b"tree");
        assert!(matches!(
            v.commit(None, &snapshot(root)).unwrap(),
            CommitOutcome::Accepted(_)
        ));

        let moved = snapshot(ObjectId::hash(b"other"));
        let moved_id = v.simulate_other_device_commit(&moved);
        match v.commit(None, &snapshot(root)).unwrap() {
            CommitOutcome::Conflict { current } => assert_eq!(current, Some(moved_id)),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert_eq!(v.rejected_commits, 1);
        assert_eq!(v.commits, 1);
    }

    #[test]
    fn commit_accepts_matching_parent() {
        let mut v = MemoryVault::new();
        let s = snapshot(ObjectId::hash(b"t"));
        let CommitOutcome::Accepted(id) = v.commit(None, &s).unwrap() else {
            panic!("first commit should be accepted");
        };
        let next = Snapshot {
            parent: Some(id),
            ..snapshot(ObjectId::hash(b"t2"))
        };
        assert!(matches!(
            v.commit(Some(id), &next).unwrap(),
            CommitOutcome::Accepted(_)
        ));
        assert_eq!(v.commits, 2);
    }

    #[test]
    fn objects_are_deduplicated_server_side() {
        let mut v = MemoryVault::new();
        let id = ObjectId::hash(b"data");
        assert!(v.put_object(&id, b"data").unwrap());
        assert!(!v.put_object(&id, b"data").unwrap(), "second put is a no-op");
        assert_eq!(v.object_count(), 1);
        assert_eq!(v.uploaded_objects, 1);
    }

    #[test]
    fn has_objects_reports_presence_in_order() {
        let mut v = MemoryVault::new();
        let a = ObjectId::hash(b"a");
        let b = ObjectId::hash(b"b");
        v.put_object(&a, b"a").unwrap();
        assert_eq!(v.has_objects(&[a, b]).unwrap(), vec![true, false]);
        assert!(v.has_objects(&[]).unwrap().is_empty());
    }

    #[test]
    fn get_object_counts_served_and_missing() {
        let mut v = MemoryVault::new();
        let a = ObjectId::hash(b"a");
        v.put_object(&a, b"a").unwrap();
        assert_eq!(v.get_object(&a).unwrap().unwrap(), b"a");
        assert!(v.get_object(&ObjectId::hash(b"nope")).unwrap().is_none());
        assert_eq!(v.served_objects, 2);
    }
}
