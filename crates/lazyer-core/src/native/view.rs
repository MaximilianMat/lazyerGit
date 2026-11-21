//! Everything the UI shows besides file status: branches with their
//! upstream distance, remote branches, tags, the commit log, stashes and
//! worktrees. Read straight from the repository files.

use super::commitobj::RawCommit;
use super::refs::{self, Head, Layout};
use super::tree::{hex, unhex, Kind, Objects, Oid};
use crate::api::{BranchInfo, CommitInfo, Result, StashInfo, WorktreeInfo};
use anyhow::Context;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// All refs below `prefix` (`refs/heads/`), name to target id. Loose refs
/// override packed ones. Annotated tags keep the id of the tag object.
pub fn list_refs(l: &Layout, prefix: &str) -> Result<BTreeMap<String, Oid>> {
    let mut out = BTreeMap::new();
    let packed = refs::packed(l);
    let start = packed.refs.partition_point(|(n, _)| n.as_str() < prefix);
    for (name, oid) in packed.refs[start..].iter().take_while(|(n, _)| n.starts_with(prefix)) {
        out.insert(name.clone(), *oid);
    }
    loose(l, &l.common_dir.join(prefix.trim_end_matches('/')), prefix.trim_end_matches('/'), &mut out)?;
    Ok(out)
}

fn loose(l: &Layout, dir: &Path, name: &str, out: &mut BTreeMap<String, Oid>) -> Result<()> {
    let Ok(read) = std::fs::read_dir(dir) else { return Ok(()) };
    for entry in read.flatten() {
        let file_name = entry.file_name();
        let Some(part) = file_name.to_str() else { continue };
        if part.ends_with(".lock") {
            continue;
        }
        let full = format!("{name}/{part}");
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            loose(l, &entry.path(), &full, out)?;
        } else if let Some(oid) = refs::resolve(l, &full)? {
            out.insert(full, oid);
        }
    }
    Ok(())
}

/// Facts about objects that can never change, kept for as long as the
/// repository is open: commits, distances between two commits, the order of
/// history below a commit, what a tag points to, and (until the configuration
/// changes) which ref a branch tracks.
#[derive(Default)]
pub struct Memo {
    commits: Mutex<HashMap<Oid, Arc<RawCommit>>>,
    ahead_behind: Mutex<HashMap<(Oid, Oid), (u32, u32)>>,
    topo: Mutex<Vec<((Oid, usize), Arc<Vec<Oid>>)>>,
    peeled: Mutex<HashMap<Oid, Oid>>,
    upstream: Mutex<HashMap<String, Option<String>>>,
}

impl Memo {
    pub fn upstream(&self, branch_ref: &str, find: impl FnOnce() -> Option<String>) -> Option<String> {
        if let Some(u) = self.upstream.lock().unwrap().get(branch_ref) {
            return u.clone();
        }
        let u = find();
        self.upstream.lock().unwrap().insert(branch_ref.to_string(), u.clone());
        u
    }

    /// The object an annotated tag finally points to.
    pub fn peeled(&self, tag: &Oid, find: impl FnOnce() -> Result<Oid>) -> Result<Oid> {
        if let Some(t) = self.peeled.lock().unwrap().get(tag) {
            return Ok(*t);
        }
        let t = find()?;
        self.peeled.lock().unwrap().insert(*tag, t);
        Ok(t)
    }
}

/// Parents, generation and commit time, from the commit-graph when the
/// commit is in it, otherwise from the object.
pub struct Commits<'a> {
    pub odb: &'a dyn Objects,
    pub graph: Option<gix::commitgraph::Graph>,
    memo: Option<&'a Memo>,
    cache: std::cell::RefCell<HashMap<Oid, std::rc::Rc<Node>>>,
}

pub struct Node {
    pub parents: Vec<Oid>,
    /// Larger than that of every ancestor; `u32::MAX` outside the graph.
    pub generation: u32,
    pub time: i64,
}

impl<'a> Commits<'a> {
    pub fn new(odb: &'a dyn Objects, common_dir: &Path) -> Self {
        let graph = gix::commitgraph::Graph::from_info_dir(&common_dir.join("objects").join("info"), gix::hash::Kind::Sha1).ok();
        Commits { odb, graph, memo: None, cache: Default::default() }
    }

    pub fn with_memo(mut self, memo: &'a Memo) -> Self {
        self.memo = Some(memo);
        self
    }

    pub fn raw(&self, oid: &Oid) -> Result<Arc<RawCommit>> {
        if let Some(c) = self.memo.and_then(|m| m.commits.lock().unwrap().get(oid).cloned()) {
            return Ok(c);
        }
        let (kind, data) = self.odb.read(oid)?;
        anyhow::ensure!(kind == Kind::Commit, "{} is not a commit", hex(oid));
        let c = Arc::new(RawCommit::parse(&data)?);
        if let Some(m) = self.memo {
            m.commits.lock().unwrap().insert(*oid, c.clone());
        }
        Ok(c)
    }

    pub fn node(&self, oid: &Oid) -> Result<std::rc::Rc<Node>> {
        if let Some(n) = self.cache.borrow().get(oid) {
            return Ok(n.clone());
        }
        let from_graph = self.graph.as_ref().and_then(|g| {
            let c = g.commit_by_id(gix::hash::ObjectId::from_bytes_or_panic(oid))?;
            let mut parents = Vec::new();
            for p in c.iter_parents() {
                let mut id = [0u8; 20];
                id.copy_from_slice(g.id_at(p.ok()?).as_bytes());
                parents.push(id);
            }
            Some(Node { parents, generation: c.generation(), time: c.committer_timestamp() as i64 })
        });
        let node = match from_graph {
            Some(n) => n,
            None => {
                let c = self.raw(oid)?;
                let time = ident_time(&c.committer);
                Node { parents: c.parents.iter().map(|p| unhex(p)).collect::<Result<_>>()?, generation: u32::MAX, time }
            }
        };
        let node = std::rc::Rc::new(node);
        self.cache.borrow_mut().insert(*oid, node.clone());
        Ok(node)
    }

    /// How many commits each side has that the other lacks.
    pub fn ahead_behind(&self, local: &Oid, upstream: &Oid) -> Result<(u32, u32)> {
        if local == upstream {
            return Ok((0, 0));
        }
        if let Some(r) = self.memo.and_then(|m| m.ahead_behind.lock().unwrap().get(&(*local, *upstream)).copied()) {
            return Ok(r);
        }
        const LOCAL: u8 = 1;
        const UPSTREAM: u8 = 2;
        const BOTH: u8 = 3;
        let key = |o: &Oid| -> Result<(u32, i64)> {
            let n = self.node(o)?;
            Ok((n.generation, n.time))
        };
        let mut flags: HashMap<Oid, u8> = HashMap::from([(*local, LOCAL), (*upstream, UPSTREAM)]);
        let mut queue: BinaryHeap<((u32, i64), Oid)> = BinaryHeap::from([(key(local)?, *local), (key(upstream)?, *upstream)]);
        // Descendants first, so a commit's colour is final when it is taken
        // out. Done once only commits reachable from both sides are left.
        while queue.iter().any(|(_, q)| flags[q] != BOTH) {
            let (_, oid) = queue.pop().unwrap();
            let f = flags[&oid];
            for p in &self.node(&oid)?.parents {
                let e = flags.entry(*p).or_insert(0);
                if *e | f != *e {
                    *e |= f;
                    queue.push((key(p)?, *p));
                }
            }
        }
        let ahead = flags.values().filter(|f| **f == LOCAL).count() as u32;
        let behind = flags.values().filter(|f| **f == UPSTREAM).count() as u32;
        if let Some(m) = self.memo {
            m.ahead_behind.lock().unwrap().insert((*local, *upstream), (ahead, behind));
        }
        Ok((ahead, behind))
    }

    /// The first `limit` commits of `git log --topo-order` from `start`:
    /// a commit is shown only after all its children, and after a merge the
    /// history of its last parent comes first.
    pub fn topo_log(&self, start: &Oid, limit: usize) -> Result<Arc<Vec<Oid>>> {
        if let Some(m) = self.memo {
            if let Some((_, v)) = m.topo.lock().unwrap().iter().find(|(k, _)| *k == (*start, limit)) {
                return Ok(v.clone());
            }
        }
        let out = Arc::new(self.topo_log_uncached(start, limit)?);
        if let Some(m) = self.memo {
            let mut topo = m.topo.lock().unwrap();
            if topo.len() >= 16 {
                topo.remove(0);
            }
            topo.push(((*start, limit), out.clone()));
        }
        Ok(out)
    }

    fn topo_log_uncached(&self, start: &Oid, limit: usize) -> Result<Vec<Oid>> {
        let mut indegree: HashMap<Oid, u32> = HashMap::new();
        let mut seen: HashSet<Oid> = HashSet::from([*start]);
        // Commits whose parents still have to be counted, highest generation first.
        let mut explore: BinaryHeap<(u32, Oid)> = BinaryHeap::from([(self.node(start)?.generation, *start)]);
        let mut stack = vec![*start];
        let mut out = Vec::with_capacity(limit);
        while let Some(oid) = stack.pop() {
            out.push(oid);
            if out.len() == limit {
                break;
            }
            let node = self.node(&oid)?;
            for p in &node.parents {
                // A parent's child count is final once every commit of a
                // higher generation has been explored.
                let generation = self.node(p)?.generation;
                while explore.peek().is_some_and(|(g, _)| *g > generation || *g == u32::MAX) {
                    let (_, c) = explore.pop().unwrap();
                    for q in &self.node(&c)?.parents {
                        *indegree.entry(*q).or_insert(0) += 1;
                        if seen.insert(*q) {
                            explore.push((self.node(q)?.generation, *q));
                        }
                    }
                }
                let n = indegree.get_mut(p).context("parent was never counted")?;
                *n -= 1;
                if *n == 0 {
                    stack.push(*p);
                }
            }
        }
        Ok(out)
    }
}

pub fn ident_time(ident: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(ident);
    let mut parts = text.rsplitn(3, ' ');
    let _tz = parts.next();
    parts.next().and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// `%s`: the first paragraph of the message on one line.
pub fn subject(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    let mut lines = text.lines().skip_while(|l| l.trim().is_empty()).take_while(|l| !l.trim().is_empty());
    let mut out = lines.next().unwrap_or("").trim_end().to_string();
    for l in lines {
        out.push(' ');
        out.push_str(l.trim_end());
    }
    out
}

pub fn commit_info(commits: &Commits, oid: &Oid) -> Result<CommitInfo> {
    let c = commits.raw(oid)?;
    let author = String::from_utf8_lossy(&c.author).to_string();
    let (name, rest) = author.split_once(" <").unwrap_or((author.as_str(), ""));
    let (email, _) = rest.split_once('>').unwrap_or((rest, ""));
    Ok(CommitInfo {
        oid: hex(oid),
        parents: c.parents.clone(),
        author_name: name.to_string(),
        author_email: email.to_string(),
        author_time: ident_time(&c.author),
        subject: subject(&c.message),
    })
}

pub fn branches(l: &Layout, commits: &Commits, head: &Head, upstream_of: &dyn Fn(&str) -> Option<String>) -> Result<Vec<BranchInfo>> {
    let current = match head {
        Head::Branch(name, _) => Some(name.as_str()),
        Head::Detached(_) => None,
    };
    let mut out = Vec::new();
    for (name, oid) in list_refs(l, "refs/heads/")? {
        let c = commits.raw(&oid)?;
        let upstream_ref = upstream_of(&name);
        let ahead_behind = match &upstream_ref {
            Some(up) => match refs::resolve(l, up)? {
                Some(up_oid) => Some(commits.ahead_behind(&oid, &up_oid)?),
                None => None,
            },
            None => None,
        };
        out.push(BranchInfo {
            name: name["refs/heads/".len()..].to_string(),
            oid: hex(&oid),
            is_head: current == Some(name.as_str()),
            upstream: upstream_ref.map(|u| short_upstream(&u)),
            ahead_behind,
            subject: subject(&c.message),
            committer_time: ident_time(&c.committer),
        });
    }
    out.sort();
    Ok(out)
}

pub fn stashes(l: &Layout) -> Result<Vec<StashInfo>> {
    let Ok(text) = std::fs::read_to_string(l.common_dir.join("logs/refs/stash")) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for line in text.lines().rev() {
        let (head, message) = line.split_once('\t').unwrap_or((line, ""));
        if let Some(oid) = head.split(' ').nth(1) {
            out.push(StashInfo { oid: oid.to_string(), message: message.to_string() });
        }
    }
    Ok(out)
}

pub fn worktrees(l: &Layout) -> Result<Vec<WorktreeInfo>> {
    let mut out = Vec::new();
    for (root, git_dir) in refs::worktrees(l)? {
        let wl = Layout { worktree: root.clone(), git_dir, common_dir: l.common_dir.clone() };
        let Ok(head) = refs::read_head(&wl) else { continue };
        let (oid, branch) = match head {
            Head::Branch(name, oid) => (oid, Some(name)),
            Head::Detached(oid) => (Some(oid), None),
        };
        out.push(WorktreeInfo { path: root.to_string_lossy().to_string(), head: oid.map(|o| hex(&o)).unwrap_or_default(), branch });
    }
    out.sort();
    Ok(out)
}

/// `origin/x` for `refs/remotes/origin/x`, `x` for a local upstream.
pub fn short_upstream(full: &str) -> String {
    full.strip_prefix("refs/remotes/").or_else(|| full.strip_prefix("refs/heads/")).unwrap_or(full).to_string()
}
