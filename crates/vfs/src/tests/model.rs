//! Randomised differential test against a small reference model.
//!
//! The model is an arena of files and directories addressed by normalised
//! component lists (no symlinks, `.` or `..`), with the Linux error rules
//! for each operation written out independently of the implementation.
//! Random operations on a tiny namespace (`a`/`b`/`c`, depth <= 3) hit the
//! collisions and corner cases often; after every step the whole tree,
//! link counts, contents, and open-but-unlinked files are compared.

use super::*;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use patina_abi::fs::{RENAME_EXCHANGE, RENAME_NOREPLACE};
use std::format;

#[derive(Clone, Debug)]
enum MNode {
    File(Vec<u8>),
    Dir(BTreeMap<String, usize>),
}

type MRes<T> = core::result::Result<T, i32>;

struct Model {
    nodes: Vec<MNode>,
}

const ROOT: usize = 0;

#[derive(Clone, Debug)]
struct P {
    comps: Vec<String>,
    trailing: bool,
}

impl P {
    fn bytes(&self) -> Vec<u8> {
        let mut s = String::new();
        for c in &self.comps {
            s.push('/');
            s.push_str(c);
        }
        if self.trailing {
            s.push('/');
        }
        s.into_bytes()
    }
    fn parent(&self) -> &[String] {
        &self.comps[..self.comps.len() - 1]
    }
    fn last(&self) -> &str {
        self.comps.last().unwrap()
    }
}

impl Model {
    fn new() -> Self {
        Model {
            nodes: vec![MNode::Dir(BTreeMap::new())],
        }
    }
    fn is_dir(&self, i: usize) -> bool {
        matches!(self.nodes[i], MNode::Dir(_))
    }
    fn dir(&self, i: usize) -> &BTreeMap<String, usize> {
        match &self.nodes[i] {
            MNode::Dir(d) => d,
            _ => panic!("not a dir"),
        }
    }
    fn dir_mut(&mut self, i: usize) -> &mut BTreeMap<String, usize> {
        match &mut self.nodes[i] {
            MNode::Dir(d) => d,
            _ => panic!("not a dir"),
        }
    }
    fn file_mut(&mut self, i: usize) -> &mut Vec<u8> {
        match &mut self.nodes[i] {
            MNode::File(d) => d,
            _ => panic!("not a file"),
        }
    }
    fn add(&mut self, n: MNode) -> usize {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    /// Walk directory components.
    fn walk(&self, comps: &[String]) -> MRes<usize> {
        let mut cur = ROOT;
        for c in comps {
            match self.dir(cur).get(c) {
                None => return Err(ENOENT),
                Some(&i) if !self.is_dir(i) => return Err(ENOTDIR),
                Some(&i) => cur = i,
            }
        }
        Ok(cur)
    }

    /// Full lookup of an existing node.
    fn lookup(&self, p: &P) -> MRes<usize> {
        let d = self.walk(p.parent())?;
        let i = *self.dir(d).get(p.last()).ok_or(ENOENT)?;
        if p.trailing && !self.is_dir(i) {
            return Err(ENOTDIR);
        }
        Ok(i)
    }

    fn create(&mut self, p: &P, excl: bool) -> MRes<usize> {
        let d = self.walk(p.parent())?;
        if p.trailing {
            return Err(EISDIR);
        }
        match self.dir(d).get(p.last()) {
            Some(_) if excl => Err(EEXIST),
            Some(&i) if self.is_dir(i) => Err(EISDIR),
            Some(&i) => Ok(i),
            None => {
                let i = self.add(MNode::File(Vec::new()));
                self.dir_mut(d).insert(p.last().to_string(), i);
                Ok(i)
            }
        }
    }

    fn mkdir(&mut self, p: &P) -> MRes<()> {
        let d = self.walk(p.parent())?;
        if self.dir(d).contains_key(p.last()) {
            return Err(EEXIST);
        }
        let i = self.add(MNode::Dir(BTreeMap::new()));
        self.dir_mut(d).insert(p.last().to_string(), i);
        Ok(())
    }

    fn unlink(&mut self, p: &P) -> MRes<()> {
        let d = self.walk(p.parent())?;
        let i = *self.dir(d).get(p.last()).ok_or(ENOENT)?;
        if self.is_dir(i) {
            return Err(EISDIR);
        }
        if p.trailing {
            return Err(ENOTDIR);
        }
        self.dir_mut(d).remove(p.last());
        Ok(())
    }

    fn rmdir(&mut self, p: &P) -> MRes<()> {
        let d = self.walk(p.parent())?;
        let i = *self.dir(d).get(p.last()).ok_or(ENOENT)?;
        if !self.is_dir(i) {
            return Err(ENOTDIR);
        }
        if !self.dir(i).is_empty() {
            return Err(ENOTEMPTY);
        }
        self.dir_mut(d).remove(p.last());
        Ok(())
    }

    fn link(&mut self, a: &P, b: &P) -> MRes<()> {
        let i = self.lookup(a)?;
        let d = self.walk(b.parent())?;
        if self.dir(d).contains_key(b.last()) {
            return Err(EEXIST);
        }
        if b.trailing {
            return Err(ENOENT);
        }
        if self.is_dir(i) {
            return Err(EPERM);
        }
        self.dir_mut(d).insert(b.last().to_string(), i);
        Ok(())
    }

    fn rename(&mut self, a: &P, b: &P, flags: u32) -> MRes<()> {
        let exch = flags & RENAME_EXCHANGE != 0;
        let od = self.walk(a.parent())?;
        let nd = self.walk(b.parent())?;
        let old = *self.dir(od).get(a.last()).ok_or(ENOENT)?;
        let new = self.dir(nd).get(b.last()).copied();
        if flags & RENAME_NOREPLACE != 0 && new.is_some() {
            return Err(EEXIST);
        }
        let old_dir = self.is_dir(old);
        let new_dir = new.is_some_and(|n| self.is_dir(n));
        if exch {
            if new.is_none() {
                return Err(ENOENT);
            }
            if !new_dir && b.trailing {
                return Err(ENOTDIR);
            }
        }
        if !old_dir && (a.trailing || (!exch && b.trailing)) {
            return Err(ENOTDIR);
        }
        if a.parent() != b.parent() {
            let prefix = |x: &[String], y: &[String]| y.len() >= x.len() && &y[..x.len()] == x;
            if old_dir && prefix(&a.comps, b.parent()) {
                return Err(EINVAL);
            }
            if new_dir && prefix(&b.comps, a.parent()) {
                return Err(if exch { EINVAL } else { ENOTEMPTY });
            }
        }
        if new == Some(old) {
            return Ok(());
        }
        if let (Some(n), false) = (new, exch) {
            if old_dir && !new_dir {
                return Err(ENOTDIR);
            }
            if !old_dir && new_dir {
                return Err(EISDIR);
            }
            if new_dir && !self.dir(n).is_empty() {
                return Err(ENOTEMPTY);
            }
        }
        if exch {
            let n = new.unwrap();
            self.dir_mut(od).insert(a.last().to_string(), n);
            self.dir_mut(nd).insert(b.last().to_string(), old);
        } else {
            self.dir_mut(od).remove(a.last());
            self.dir_mut(nd).insert(b.last().to_string(), old);
        }
        Ok(())
    }

    /// Link count of every node reachable from the root.
    fn nlinks(&self) -> BTreeMap<usize, u32> {
        let mut m = BTreeMap::new();
        m.insert(ROOT, 2);
        let mut stack = vec![ROOT];
        while let Some(d) = stack.pop() {
            for &c in self.dir(d).values() {
                if self.is_dir(c) {
                    *m.entry(d).or_insert(2) += 1;
                    m.insert(c, 2);
                    stack.push(c);
                } else {
                    *m.entry(c).or_insert(0) += 1;
                }
            }
        }
        m
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn path(&mut self) -> P {
        let depth = 1 + self.below(3) as usize;
        let comps = (0..depth)
            .map(|_| ["a", "b", "c"][self.below(3) as usize].to_string())
            .collect();
        P {
            comps,
            trailing: self.below(8) == 0,
        }
    }
}

fn res<T>(r: Result<T>) -> MRes<()> {
    r.map(|_| ()).map_err(|e| e.0)
}

/// Compare the whole tree; returns model index -> ino.
fn compare(fs: &mut Fs, m: &Model, ctx: &str) -> BTreeMap<usize, Ino> {
    let nl = m.nlinks();
    let mut map: BTreeMap<usize, Ino> = BTreeMap::new();
    map.insert(ROOT, R);
    let mut stack = vec![(ROOT, R, String::from("/"))];
    while let Some((mi, ino, path)) = stack.pop() {
        let st = fs.stat(ino).unwrap();
        assert_eq!(st.nlink, nl[&mi], "{ctx}: nlink of {path}");
        match &m.nodes[mi] {
            MNode::File(data) => {
                assert_eq!(
                    fs.file_type(ino).unwrap(),
                    FileType::Regular,
                    "{ctx}: {path}"
                );
                assert_eq!(
                    &read_all(fs, ino).unwrap(),
                    data,
                    "{ctx}: contents of {path}"
                );
            }
            MNode::Dir(d) => {
                assert_eq!(
                    fs.file_type(ino).unwrap(),
                    FileType::Directory,
                    "{ctx}: {path}"
                );
                let mut got: Vec<(Vec<u8>, Ino)> = list(fs, ino, 64)
                    .unwrap()
                    .into_iter()
                    .filter(|(n, _, _)| n != b"." && n != b"..")
                    .map(|(n, i, _)| (n, i))
                    .collect();
                got.sort();
                let want: Vec<&String> = d.keys().collect();
                let got_names: Vec<String> = got
                    .iter()
                    .map(|(n, _)| String::from_utf8(n.clone()).unwrap())
                    .collect();
                assert_eq!(
                    got_names.iter().collect::<Vec<_>>(),
                    want,
                    "{ctx}: entries of {path}"
                );
                assert_eq!(st.size, (d.len() as u64 + 2) * 20);
                if mi != ROOT {
                    assert_eq!(
                        fs.getcwd(ino).unwrap(),
                        path.trim_end_matches('/').as_bytes()
                    );
                }
                for (name, ci) in got {
                    let name = String::from_utf8(name).unwrap();
                    let mc = d[&name];
                    if let Some(&prev) = map.get(&mc) {
                        assert_eq!(prev, ci, "{ctx}: hard link identity at {path}{name}");
                    } else {
                        assert!(!map.values().any(|&v| v == ci), "{ctx}: ino reused");
                        map.insert(mc, ci);
                    }
                    if m.is_dir(mc) {
                        stack.push((mc, ci, format!("{path}{name}/")));
                    } else {
                        stack.push((mc, ci, format!("{path}{name}")));
                    }
                }
            }
        }
    }
    map
}

fn run(seed: u64, steps: usize, seen: &mut alloc::collections::BTreeSet<(u64, MRes<()>)>) {
    let mut rng = Rng(seed | 1);
    let mut fs = new_fs();
    let mut m = Model::new();
    // Open handles: (model index, ino).
    let mut open: Vec<(usize, Ino)> = Vec::new();
    for step in 0..steps {
        let op = rng.below(12);
        let a = rng.path();
        let ab = a.bytes();
        let ctx = format!(
            "seed {seed} step {step} op {op} {}",
            String::from_utf8_lossy(&ab)
        );
        match op {
            0 | 1 => {
                let excl = rng.below(3) == 0;
                let o = OpenOptions { excl, ..ow() };
                let want = m.create(&a, excl);
                let got = fs.open(R, &ab, &o);
                assert_eq!(res(got), want.map(|_| ()), "{ctx}: create");
                seen.insert((0, res(got)));
                if let (Ok(o), Ok(mi)) = (got, want) {
                    if rng.below(3) == 0 {
                        open.push((mi, o.ino));
                    } else {
                        fs.put_ref(o.ino);
                    }
                }
            }
            2 => {
                let got = res(fs.mkdir(R, &ab, 0o755));
                assert_eq!(got, m.mkdir(&a), "{ctx}: mkdir");
                seen.insert((2, got));
            }
            3 => {
                let got = res(fs.unlink(R, &ab));
                assert_eq!(got, m.unlink(&a), "{ctx}: unlink");
                seen.insert((3, got));
            }
            4 => {
                let got = res(fs.rmdir(R, &ab));
                assert_eq!(got, m.rmdir(&a), "{ctx}: rmdir");
                seen.insert((4, got));
            }
            5 | 6 => {
                let b = rng.path();
                let flags = [0, 0, RENAME_NOREPLACE, RENAME_EXCHANGE][rng.below(4) as usize];
                let got = fs.rename(R, &ab, R, &b.bytes(), flags);
                let want = m.rename(&a, &b, flags);
                seen.insert((5, res(got)));
                assert_eq!(
                    res(got),
                    want,
                    "{ctx}: rename -> {} flags {flags}",
                    String::from_utf8_lossy(&b.bytes())
                );
            }
            7 => {
                let b = rng.path();
                let got = fs.link(R, &ab, false, R, &b.bytes());
                assert_eq!(res(got), m.link(&a, &b), "{ctx}: link");
                seen.insert((7, res(got)));
            }
            8 => {
                // Write through a path.
                let off = rng.below(9000);
                let len = rng.below(5000) as usize;
                let byte = rng.below(256) as u8;
                let data = vec![byte; len];
                match (fs.resolve(R, &ab, true), m.lookup(&a)) {
                    (Ok(ino), Ok(mi)) => {
                        let r = fs.write_at(ino, off, &data);
                        if m.is_dir(mi) {
                            assert_eq!(r, Err(err(EISDIR)), "{ctx}");
                        } else {
                            r.unwrap();
                            let f = m.file_mut(mi);
                            let end = off as usize + len;
                            if len > 0 && f.len() < end {
                                f.resize(end, 0);
                            }
                            f[off as usize..off as usize + len].copy_from_slice(&data);
                        }
                    }
                    (got, want) => assert_eq!(res(got), want.map(|_| ()), "{ctx}: resolve"),
                }
            }
            9 => {
                let len = rng.below(12000);
                match (fs.resolve(R, &ab, true), m.lookup(&a)) {
                    (Ok(ino), Ok(mi)) => {
                        let r = fs.truncate(ino, len);
                        if m.is_dir(mi) {
                            assert_eq!(r, Err(err(EISDIR)));
                        } else {
                            r.unwrap();
                            m.file_mut(mi).resize(len as usize, 0);
                        }
                    }
                    (got, want) => assert_eq!(res(got), want.map(|_| ()), "{ctx}: resolve"),
                }
            }
            10 => {
                // Use or close an open handle (possibly unlinked).
                if !open.is_empty() {
                    let k = rng.below(open.len() as u64) as usize;
                    let (mi, ino) = open[k];
                    match rng.below(3) {
                        0 => {
                            let (n, _) = fs.append(ino, b"+h").unwrap();
                            assert_eq!(n, 2);
                            m.file_mut(mi).extend_from_slice(b"+h");
                        }
                        1 => {
                            let want = match &m.nodes[mi] {
                                MNode::File(d) => d.clone(),
                                _ => unreachable!(),
                            };
                            assert_eq!(read_all(&mut fs, ino).unwrap(), want, "{ctx}: handle read");
                        }
                        _ => {
                            fs.put_ref(ino);
                            open.swap_remove(k);
                        }
                    }
                }
            }
            _ => {
                // Plain lookup / stat.
                let got = fs.resolve(R, &ab, true);
                assert_eq!(res(got), m.lookup(&a).map(|_| ()), "{ctx}: lookup");
            }
        }
        if step % 16 == 0 || step + 1 == steps {
            compare(&mut fs, &m, &ctx);
        }
    }
    // Close everything: only reachable inodes remain.
    for (_, ino) in open.drain(..) {
        fs.put_ref(ino);
    }
    let map = compare(&mut fs, &m, &format!("seed {seed} final"));
    assert_eq!(fs.inode_count(), map.len(), "seed {seed}: leaked inodes");
    let pages: u64 = map
        .values()
        .filter(|&&i| fs.file_type(i).unwrap() == FileType::Regular)
        .map(|&i| fs.stat(i).unwrap().blocks / 8)
        .sum();
    assert_eq!(fs.pages_used(), pages, "seed {seed}: page accounting");
}

#[test]
fn randomized_against_model() {
    let mut seen = alloc::collections::BTreeSet::new();
    for seed in 1..=300u64 {
        run(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15), 300, &mut seen);
    }
    // The generator reaches every interesting outcome.
    for want in [
        (0, Ok(())),
        (0, Err(EEXIST)),
        (0, Err(EISDIR)),
        (0, Err(ENOTDIR)),
        (0, Err(ENOENT)),
        (2, Ok(())),
        (2, Err(EEXIST)),
        (3, Ok(())),
        (3, Err(EISDIR)),
        (3, Err(ENOTDIR)),
        (4, Ok(())),
        (4, Err(ENOTEMPTY)),
        (4, Err(ENOTDIR)),
        (5, Ok(())),
        (5, Err(EINVAL)),
        (5, Err(ENOTEMPTY)),
        (5, Err(EEXIST)),
        (5, Err(EISDIR)),
        (5, Err(ENOTDIR)),
        (5, Err(ENOENT)),
        (7, Ok(())),
        (7, Err(EPERM)),
        (7, Err(EEXIST)),
    ] {
        assert!(seen.contains(&want), "never observed {want:?}");
    }
}

#[test]
fn randomized_long_run() {
    run(0xdead_beef, 5000, &mut alloc::collections::BTreeSet::new());
}
