use crate::config::{self, WorkerConfig};
use crate::provider::{WorkerProvider, boat::BoatProvider};
use crate::snapshot::compute_worktree_id;
use crate::state::{JobRecord, atomic_write};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

fn lock_file(path: &Path) -> Result<File> {
    Ok(File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}
fn try_lock(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Lease {
    id: String,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Fork {
    sandbox: String,
    #[serde(default)]
    deleting: bool,
    created_at: DateTime<Utc>,
    lease: Option<Lease>,
    last_job: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    forks: Vec<Fork>,
    queue: Vec<Ticket>,
    base_last_job: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Ticket {
    job: String,
    worktree: String,
}

pub struct Pool {
    dir: PathBuf,
    base: BoatProvider,
    cap: usize,
    reserved: Vec<String>,
}

// Keep the local activity lock from admission through sync, build, pull and sign.
pub struct Assignment {
    pub provider: BoatProvider,
    pub created: Option<Instant>,
    activity: File,
}

impl Drop for Assignment {
    fn drop(&mut self) {
        // A concurrently spawned child can briefly inherit a descriptor before exec.
        // End the lease at job completion even if that descriptor is still open.
        let _ = self.activity.unlock();
    }
}

impl Pool {
    pub fn configured(
        name: &str,
        cfg: &WorkerConfig,
        global: &config::GlobalConfig,
    ) -> Result<Option<Self>> {
        let WorkerConfig::Boat {
            sandbox_id,
            pool: Some(settings),
            hourly_rate_usd,
            ssh_key,
            ..
        } = cfg
        else {
            return Ok(None);
        };
        let namespace = hex::encode(Sha256::digest(format!("{name}\0{sandbox_id}")));
        let dir = config::state_dir()?.join("pools").join(namespace);
        fs::create_dir_all(&dir)?;
        let reserved = global
            .workers
            .values()
            .filter_map(|w| match w {
                WorkerConfig::Boat { sandbox_id, .. } => Some(sandbox_id.clone()),
                _ => None,
            })
            .collect();
        Ok(Some(Self {
            dir,
            cap: settings.max_forks.get(),
            reserved,
            base: BoatProvider::new(sandbox_id.clone(), *hourly_rate_usd, ssh_key.clone()),
        }))
    }

    fn guard(&self, no_wait: bool) -> Result<File> {
        let lock = lock_file(&self.dir.join("pool.lock"))?;
        if no_wait {
            ensure!(try_lock(&lock)?, "pool admission is busy (--no-wait)");
        } else {
            lock.lock()?;
        }
        Ok(lock)
    }

    fn read(&self) -> Result<Registry> {
        match fs::read(self.dir.join("registry.json")) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, state: &Registry) -> Result<()> {
        atomic_write(
            &self.dir.join("registry.json"),
            &serde_json::to_vec_pretty(state)?,
        )
    }
    fn child(&self, id: &str) -> Result<BoatProvider> {
        ensure!(
            id.strip_prefix("bx_")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
                && !self.reserved.iter().any(|s| s == id),
            "refusing invalid or reserved fork ID {id}"
        );
        Ok(BoatProvider::new(
            id.into(),
            self.base.hourly_rate_usd,
            self.base.ssh_key.clone(),
        ))
    }
    fn activity(&self, id: &str) -> Result<File> {
        lock_file(
            &self
                .dir
                .join(format!("{}.lock", hex::encode(Sha256::digest(id)))),
        )
    }

    // Boat writes its reply directly to disk. If the caller dies after creation,
    // the next holder of the registry lock can recover ownership without guessing from boat list.
    fn recover(&self, state: &mut Registry) -> Result<()> {
        let path = self.dir.join("fork-reply");
        if path.exists() {
            let receipt = lock_file(&path)?;
            ensure!(
                try_lock(&receipt)?,
                "previous fork request still in progress"
            );
            let raw = fs::read_to_string(&path)?;
            let id = BoatProvider::fork_id(&raw).context(
                "incomplete Boat fork receipt; inspect fork-reply before allocating again",
            )?;
            self.child(&id)?;
            if !state.forks.iter().any(|f| f.sandbox == id) {
                state.forks.push(Fork {
                    sandbox: id,
                    deleting: false,
                    created_at: Utc::now(),
                    lease: None,
                    last_job: None,
                });
                self.save(state)?;
            }
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn prune_queue(&self, state: &mut Registry) -> Result<()> {
        let mut live = Vec::new();
        for ticket in state.queue.drain(..) {
            let path = self.dir.join(&ticket.job);
            if path.is_file() {
                let file = lock_file(&path)?;
                if !try_lock(&file)? {
                    live.push(ticket);
                    continue;
                }
                fs::remove_file(path)?;
            }
        }
        state.queue = live;
        Ok(())
    }

    fn reclaim(&self, state: &mut Registry, release: Option<&Path>) -> Result<Vec<String>> {
        let mut deleted = Vec::new();
        for i in (0..state.forks.len()).rev() {
            let f = &state.forks[i];
            let remove = match (&f.lease, release) {
                (Some(l), Some(path)) => l.path == path,
                (_, Some(_)) => false,
                (Some(l), None) => f.deleting || !l.path.try_exists()?,
                (None, None) => true,
            };
            if !remove {
                continue;
            }
            let child = self.child(&f.sandbox)?;
            let activity = self.activity(&f.sandbox)?;
            if !try_lock(&activity)? {
                ensure!(release.is_none(), "sandbox {} has an active job", f.sandbox);
                continue;
            }
            // Persist deletion intent so a crash or transient error is recoverable by gc.
            state.forks[i].deleting = true;
            self.save(state)?;
            child.delete()?;
            deleted.push(state.forks.remove(i).sandbox);
            self.save(state)?;
        }
        Ok(deleted)
    }

    pub fn gc(&self, release: Option<&Path>) -> Result<Vec<String>> {
        let _guard = self.guard(false)?;
        let mut state = self.read()?;
        self.recover(&mut state)?;
        self.reclaim(&mut state, release)
    }

    pub fn base_job(&self, job: &JobRecord, no_wait: bool) -> Result<Assignment> {
        let guard = self.activity(&self.base.sandbox_id)?;
        if no_wait {
            ensure!(try_lock(&guard)?, "base has an active job");
        } else {
            guard.lock()?;
        }
        let _pool = self.guard(false)?;
        let mut state = self.read()?;
        state.base_last_job = Some(job.id.clone());
        self.save(&state)?;
        Ok(Assignment {
            provider: self.base.clone(),
            created: None,
            activity: guard,
        })
    }

    pub fn acquire(&self, root: &Path, no_wait: bool, job: &mut JobRecord) -> Result<Assignment> {
        let started = Instant::now();
        let worktree = compute_worktree_id(root)?;
        let path = root.canonicalize()?;
        let ticket_path = self.dir.join(&job.id);
        let ticket = lock_file(&ticket_path)?;
        ticket.lock()?;
        let mut announced = false;
        let result = (|| loop {
            let _pool = self.guard(no_wait)?;
            let mut state = self.read()?;
            self.recover(&mut state)?;
            self.prune_queue(&mut state)?;
            self.reclaim(&mut state, None)?;
            let existing = state.forks.iter().position(|f| {
                f.lease
                    .as_ref()
                    .is_some_and(|l| l.id == worktree && l.path == path)
            });
            let ahead = state.ahead(&job.id, &worktree, existing.is_some());
            let mut created = None;
            let mut chosen = None;
            if ahead == 0 {
                if let Some(i) = existing {
                    let lock = self.activity(&state.forks[i].sandbox)?;
                    if try_lock(&lock)? {
                        chosen = Some((i, lock));
                    }
                } else if state.forks.len() < self.cap {
                    let base = self.activity(&self.base.sandbox_id)?;
                    if try_lock(&base)? {
                        job.stage = "fork".into();
                        created = Some(Instant::now());
                        let receipt_path = self.dir.join("fork-reply");
                        let receipt = File::create_new(&receipt_path)?;
                        receipt.lock()?;
                        self.base.fork(&receipt_path, receipt)?;
                        let id = BoatProvider::fork_id(&fs::read_to_string(&receipt_path)?)?;
                        self.child(&id)?;
                        let lock = self.activity(&id)?;
                        lock.lock()?;
                        state.forks.push(Fork {
                            sandbox: id,
                            deleting: false,
                            created_at: Utc::now(),
                            lease: Some(Lease {
                                id: worktree.clone(),
                                path: path.clone(),
                            }),
                            last_job: None,
                        });
                        self.save(&state)?;
                        fs::remove_file(receipt_path)?;
                        chosen = Some((state.forks.len() - 1, lock));
                    }
                }
            }
            if let Some((i, lock)) = chosen {
                let f = &mut state.forks[i];
                f.last_job = Some(job.id.clone());
                let provider = self.child(&f.sandbox)?;
                state.queue.retain(|t| t.job != job.id);
                self.save(&state)?;
                return Ok(Assignment {
                    provider,
                    created,
                    activity: lock,
                });
            }
            ensure!(!no_wait, "pool is full or sandbox is busy (--no-wait)");
            if !announced {
                eprintln!(
                    "queued behind {} builds",
                    ahead
                        + if existing.is_some() {
                            1
                        } else {
                            state.forks.len().max(1)
                        }
                );
                state.queue.push(Ticket {
                    job: job.id.clone(),
                    worktree: worktree.clone(),
                });
                self.save(&state)?;
                announced = true;
            }
            drop(_pool);
            thread::sleep(Duration::from_millis(200));
        })();
        let _ = fs::remove_file(ticket_path);
        job.queue_ms = result
            .as_ref()
            .ok()
            .and_then(|a| a.created)
            .unwrap_or_else(Instant::now)
            .duration_since(started)
            .as_millis() as u64;
        result
    }

    pub fn stop(&self) -> Result<Vec<String>> {
        let _guard = self.guard(false)?;
        let mut state = self.read()?;
        self.recover(&mut state)?;
        let mut idle = Vec::new();
        for provider in state
            .forks
            .iter()
            .map(|f| self.child(&f.sandbox))
            .chain(std::iter::once(Ok(self.base.clone())))
        {
            let provider = provider?;
            let activity = self.activity(&provider.sandbox_id)?;
            if try_lock(&activity)? {
                idle.push((provider, activity));
            }
        }
        drop(_guard);
        let mut stopped = Vec::new();
        let mut errors = Vec::new();
        for (provider, _activity) in idle {
            match provider.stop() {
                Ok(()) => stopped.push(provider.sandbox_id),
                Err(e) => errors.push(format!("{}: {e:#}", provider.sandbox_id)),
            }
        }
        ensure!(errors.is_empty(), "stop failed: {}", errors.join("; "));
        Ok(stopped)
    }

    pub fn status(&self) -> Result<Value> {
        let _guard = self.guard(false)?;
        let mut state = self.read()?;
        self.recover(&mut state)?;
        let mut forks = Vec::new();
        for f in &state.forks {
            forks.push(json!({"sandbox_id":f.sandbox,"state":self.child(&f.sandbox)?.state()?, "lease":f.lease,
                "last_job":f.last_job, "forked_at":f.created_at,"age_seconds":(Utc::now()-f.created_at).num_seconds()}));
        }
        Ok(
            json!({"base":{"sandbox_id":self.base.sandbox_id, "state":self.base.state()?, "last_job":state.base_last_job}, "forks":forks,"max_forks":self.cap}),
        )
    }
}

impl Registry {
    // A request for an existing lease needs no free slot. Only earlier requests
    // for that same lease precede it; a full pool must not block its own tenants.
    fn ahead(&self, job: &str, worktree: &str, existing: bool) -> usize {
        self.queue
            .iter()
            .take_while(|t| t.job != job)
            .filter(|t| {
                if existing {
                    t.worktree == worktree
                } else {
                    !self
                        .forks
                        .iter()
                        .any(|f| f.lease.as_ref().is_some_and(|l| l.id == t.worktree))
                }
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sandbox(dir: &Path) -> Pool {
        Pool {
            dir: dir.to_path_buf(),
            base: BoatProvider::new("bx_base".into(), 0., None),
            cap: 1,
            reserved: vec!["bx_base".into(), "bx_unrelated".into()],
        }
    }
    fn assigned(path: &Path) -> Registry {
        Registry {
            forks: vec![Fork {
                sandbox: "bx_owned".into(),
                deleting: false,
                created_at: Utc::now(),
                lease: Some(Lease {
                    id: compute_worktree_id(path).unwrap(),
                    path: path.canonicalize().unwrap(),
                }),
                last_job: None,
            }],
            ..Registry::default()
        }
    }
    #[test]
    fn lease_is_exclusive_reusable_and_counts_while_idle() {
        let dir = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let p = sandbox(dir.path());
        p.save(&assigned(wt.path())).unwrap();
        let first = p
            .acquire(wt.path(), false, &mut JobRecord::new("build"))
            .unwrap();
        assert_eq!(first.provider.sandbox_id, "bx_owned");
        assert!(
            p.acquire(wt.path(), true, &mut JobRecord::new("run"))
                .is_err()
        );
        let _inherited_descriptor = first.activity.try_clone().unwrap();
        drop(first);
        assert!(
            p.acquire(other.path(), true, &mut JobRecord::new("build"))
                .is_err()
        );
        assert!(
            p.acquire(wt.path(), true, &mut JobRecord::new("exec"))
                .unwrap()
                .created
                .is_none()
        );
        assert_eq!(p.read().unwrap().forks.len(), 1);
    }
    #[test]
    fn queued_requests_are_fifo_and_dead_waiters_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let p = sandbox(dir.path());
        let mut state = Registry::default();
        let locks: Vec<_> = ["first", "second", "dead"]
            .into_iter()
            .map(|id| {
                let lock = lock_file(&dir.path().join(id)).unwrap();
                lock.lock().unwrap();
                state.queue.push(Ticket {
                    job: id.into(),
                    worktree: id.into(),
                });
                lock
            })
            .collect();
        assert_eq!(state.ahead("second", "second", false), 1);
        assert_eq!(state.ahead("later", "first", true), 1);
        drop(locks);
        p.prune_queue(&mut state).unwrap();
        assert!(state.queue.is_empty());
    }
    #[test]
    fn recover_after_fork_before_lease_commit_protects_reserved_sandboxes() {
        let dir = tempfile::tempdir().unwrap();
        let p = sandbox(dir.path());
        let mut state = Registry::default();
        fs::write(dir.path().join("fork-reply"), r#"{"id":"bx_new"}"#).unwrap();
        p.recover(&mut state).unwrap();
        assert_eq!(state.forks[0].sandbox, "bx_new");
        assert!(state.forks[0].lease.is_none());
        for id in ["bx_base", "bx_unrelated", "../bad"] {
            assert!(p.child(id).is_err());
        }
    }
}
