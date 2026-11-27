//! The shared backend: fragments in Redis, for several proxies behind one
//! load balancer.
//!
//! A handle minted by one instance has to resolve on whichever instance the
//! model's retrieval lands on, and SQLite is one file on one host. Headroom
//! keeps its CCR entries in Redis for the same reason. See
//! docs/design/HEADROOM-PORT.md, Tier 4.
//!
//! # No client crate
//!
//! The store trait is synchronous and every caller is already on a blocking
//! thread, so the client is RESP2 over a `std::net::TcpStream`: connect,
//! `AUTH`, `SELECT`, pipelined commands, a small pool, a reconnect on any I/O
//! error. A dozen commands are used. TLS (`rediss://`) is refused rather than
//! half-supported; terminate it with a sidecar or keep Redis on the private
//! network.
//!
//! # Layout
//!
//! One hash per fragment at `{prefix}:r:{tenant-hex}:{handle}` holding
//! `fragment`, `kind`, `request_id`, `created_at`, `last_accessed`, `ttl` and
//! `max_life`. The tenant is hex-encoded so a tenant name can never put `:`,
//! `*` or `?` into a key or a `SCAN` pattern — the tenant is half of the key,
//! as it is in the other backends, so a handle from another tenant addresses a
//! key that does not exist.
//!
//! # Expiry
//!
//! The idle window and the lifetime cap are evaluated here on every read,
//! exactly as the other backends do, so `Lookup::Expired` stays distinct from
//! `Lookup::Miss`. Each write and touch also sets a Redis expiry one idle
//! window past the entry's deadline, so a dead entry nobody asks for is removed
//! by Redis itself and `purge_expired` has little to find.
//! Cleanup and touch use `EVAL` to compare timing fields and mutate atomically;
//! Redis ACLs must allow `EVAL` and the hash/expiry commands used by the script.
//! The expiry is relative (`PEXPIRE`), computed from this store's clock, so it
//! is right whatever the Redis server's clock says.
//!
//! # What this backend does not do
//!
//! - `refs.max_bytes` is not enforced: set `maxmemory` and
//!   `maxmemory-policy volatile-lru` on the server, which bounds every proxy at
//!   once instead of each one guessing at the others.
//! - Browsing (`/v1/refs`) is not offered; `browser()` is `None`.
//!
//! Upserts use one atomic script per key, preserving a live entry's birth
//! across concurrent writers and returning that timestamp to the local cache.
//! Scans visit at most 500 unique keys at a time; the deduplication set alone
//! grows with the number of keys. The pool caps all connections (including
//! those being opened) and waits at most `timeout` for a free slot.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use kompressor_engine::refs::RefId;
use parking_lot::{Condvar, Mutex};

use crate::clock::Clock;
use crate::types::{
    Lookup, PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, expires_at, is_expired,
};

/// Where a Redis server is and how to log in to it.
#[derive(Clone, PartialEq, Eq)]
pub struct RedisTarget {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub db: u32,
}

impl std::fmt::Debug for RedisTarget {
    // The password never reaches a log line or an error chain.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("db", &self.db)
            .finish()
    }
}

/// Parse `redis://[user[:password]@]host[:port][/db]`.
pub fn parse_url(url: &str) -> Result<RedisTarget, String> {
    let rest = match url.split_once("://") {
        Some(("redis", rest)) => rest,
        Some(("rediss", _)) => {
            return Err(
                "rediss:// (TLS) is not supported; terminate TLS in front of Redis or use \
                 redis:// on a private network"
                    .into(),
            );
        }
        Some((scheme, _)) => return Err(format!("unknown scheme {scheme:?}; expected redis://")),
        None => return Err("expected redis://host:port[/db]".into()),
    };
    let (auth, rest) = match rest.rsplit_once('@') {
        Some((auth, rest)) => (Some(auth), rest),
        None => (None, rest),
    };
    let (authority, db) = match rest.split_once('/') {
        Some((authority, "")) => (authority, 0),
        Some((authority, db)) => (
            authority,
            db.parse::<u32>()
                .map_err(|_| format!("database {db:?} is not a number"))?,
        ),
        None => (rest, 0),
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed
            .split_once(']')
            .ok_or("an IPv6 host needs its closing ]")?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port
                .parse()
                .map_err(|_| format!("port {port:?} is not a number"))?,
            None => 6379,
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse()
                    .map_err(|_| format!("port {port:?} is not a number"))?,
            ),
            None => (authority.to_string(), 6379),
        }
    };
    if host.is_empty() {
        return Err("no host in the Redis URL".into());
    }
    let (username, password) = match auth {
        None => (None, None),
        Some(auth) => match auth.split_once(':') {
            Some((user, pass)) => (
                Some(percent_decode(user)).filter(|u| !u.is_empty()),
                Some(percent_decode(pass)),
            ),
            None => (None, Some(percent_decode(auth))),
        },
    };
    Ok(RedisTarget {
        host,
        port,
        username,
        password,
        db,
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One RESP2 reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Reply>>),
}

impl Reply {
    fn into_result(self) -> Result<Reply, StoreError> {
        match self {
            Reply::Error(message) => Err(StoreError::Redis(message)),
            other => Ok(other),
        }
    }

    fn bulk_string(&self) -> Option<String> {
        match self {
            Reply::Bulk(Some(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            Reply::Simple(text) => Some(text.clone()),
            _ => None,
        }
    }

    fn int(&self) -> Option<i64> {
        match self {
            Reply::Int(n) => Some(*n),
            other => other.bulk_string()?.parse().ok(),
        }
    }

    fn array(self) -> Vec<Reply> {
        match self {
            Reply::Array(Some(items)) => items,
            _ => Vec::new(),
        }
    }
}

/// A command: its arguments, binary-safe.
pub type Command = Vec<Vec<u8>>;

/// Build a command from anything byte-like.
#[macro_export]
#[doc(hidden)]
macro_rules! redis_cmd {
    ($($arg:expr),+ $(,)?) => {
        vec![$(::std::convert::AsRef::<[u8]>::as_ref(&$arg).to_vec()),+]
    };
}

fn encode(command: &Command, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("*{}\r\n", command.len()).as_bytes());
    for arg in command {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Redis(format!("i/o: {error}"))
}

/// Read one reply.
pub fn read_reply(reader: &mut impl BufRead) -> Result<Reply, StoreError> {
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).map_err(io_error)?;
    if line.len() < 3 || !line.ends_with(b"\r\n") {
        return Err(StoreError::Redis("the connection closed mid-reply".into()));
    }
    let body = String::from_utf8_lossy(&line[1..line.len() - 2]).into_owned();
    let number = |text: &str| -> Result<i64, StoreError> {
        text.parse()
            .map_err(|_| StoreError::Redis(format!("malformed length {text:?}")))
    };
    match line[0] {
        b'+' => Ok(Reply::Simple(body)),
        b'-' => Ok(Reply::Error(body)),
        b':' => Ok(Reply::Int(number(&body)?)),
        b'$' => {
            let len = number(&body)?;
            if len < 0 {
                return Ok(Reply::Bulk(None));
            }
            let mut data = vec![0; len as usize + 2];
            reader.read_exact(&mut data).map_err(io_error)?;
            data.truncate(len as usize);
            Ok(Reply::Bulk(Some(data)))
        }
        b'*' => {
            let len = number(&body)?;
            if len < 0 {
                return Ok(Reply::Array(None));
            }
            let mut items = Vec::with_capacity(len as usize);
            for _ in 0..len {
                items.push(read_reply(reader)?);
            }
            Ok(Reply::Array(Some(items)))
        }
        other => Err(StoreError::Redis(format!(
            "unexpected reply type {:?}",
            other as char
        ))),
    }
}

/// One open connection.
struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    fn open(target: &RedisTarget, timeout: Duration) -> Result<Connection, StoreError> {
        let address = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(io_error)?
            .next()
            .ok_or_else(|| StoreError::Redis(format!("{} does not resolve", target.host)))?;
        let stream = TcpStream::connect_timeout(&address, timeout).map_err(io_error)?;
        stream.set_read_timeout(Some(timeout)).map_err(io_error)?;
        stream.set_write_timeout(Some(timeout)).map_err(io_error)?;
        let _ = stream.set_nodelay(true);
        let writer = stream.try_clone().map_err(io_error)?;
        let mut connection = Connection {
            reader: BufReader::new(stream),
            writer,
        };
        let mut setup: Vec<Command> = Vec::new();
        match (&target.username, &target.password) {
            (Some(user), Some(pass)) => setup.push(redis_cmd!["AUTH", user, pass]),
            (None, Some(pass)) => setup.push(redis_cmd!["AUTH", pass]),
            _ => {}
        }
        if target.db != 0 {
            setup.push(redis_cmd!["SELECT", target.db.to_string()]);
        }
        for reply in connection.pipeline(&setup)? {
            reply.into_result()?;
        }
        Ok(connection)
    }

    fn pipeline(&mut self, commands: &[Command]) -> Result<Vec<Reply>, StoreError> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for command in commands {
            encode(command, &mut out);
        }
        self.writer.write_all(&out).map_err(io_error)?;
        self.writer.flush().map_err(io_error)?;
        (0..commands.len())
            .map(|_| read_reply(&mut self.reader))
            .collect()
    }
}

/// How the store reaches Redis.
#[derive(Clone, Debug)]
pub struct RedisOptions {
    pub target: RedisTarget,
    /// Prepended to every key, so several deployments can share a server.
    pub prefix: String,
    /// Connection acquisition, connect, read and write timeout.
    pub timeout: Duration,
    /// Maximum connections, including idle and connecting ones (at least one).
    pub pool: usize,
    pub clock: Clock,
}

impl RedisOptions {
    pub fn new(url: &str) -> Result<RedisOptions, String> {
        Ok(RedisOptions {
            target: parse_url(url)?,
            prefix: "kompressor".into(),
            timeout: Duration::from_secs(2),
            pool: 8,
            clock: Clock::system(),
        })
    }
}

/// `PING` the server at `url`, for `kompressor doctor`.
pub fn ping(url: &str, timeout: Duration) -> Result<(), StoreError> {
    let target = parse_url(url).map_err(StoreError::Redis)?;
    let mut connection = Connection::open(&target, timeout)?;
    for reply in connection.pipeline(&[redis_cmd!["PING"]])? {
        reply.into_result()?;
    }
    Ok(())
}

const FIELDS: [&str; 4] = ["created_at", "last_accessed", "ttl", "max_life"];

const BATCH_SIZE: usize = 500;

const UPSERT: &str = r#"
-- Decimal strings preserve u64 timestamps/TTLs beyond Lua's exact integer range.
local max_u64 = '18446744073709551615'
local function less(a, b)
    if #a ~= #b then return #a < #b end
    return a < b
end
local function add(a, b)
    local out, carry = '', 0
    local i, j = #a, #b
    while i > 0 or j > 0 or carry > 0 do
        local n = (tonumber(a:sub(i, i)) or 0) + (tonumber(b:sub(j, j)) or 0) + carry
        out = tostring(n % 10) .. out
        carry = math.floor(n / 10)
        i, j = math.max(0, i - 1), math.max(0, j - 1)
    end
    if less(max_u64, out) then return max_u64 end
    return out
end
local function sub(a, b)
    if less(a, b) then return '0' end
    local out, borrow, j = '', 0, #b
    for i = #a, 1, -1 do
        local n = tonumber(a:sub(i, i)) - (tonumber(b:sub(j, j)) or 0) - borrow
        borrow = 0
        if n < 0 then n, borrow = n + 10, 1 end
        out = tostring(n) .. out
        j = math.max(0, j - 1)
    end
    out = out:gsub('^0+', '')
    if out == '' then return '0' end
    return out
end
local function deadline(born, accessed, ttl, life)
    local idle, absolute = add(accessed, ttl), add(born, life)
    if less(idle, absolute) then return idle end
    return absolute
end
local now, ttl, life = ARGV[1], ARGV[2], ARGV[3]
local old = redis.call('HMGET', KEYS[1], 'created_at', 'last_accessed', 'ttl', 'max_life')
local born, accessed = now, now
if old[1] and old[2] and old[3] and old[4]
    and not less(deadline(old[1], old[2], old[3], old[4]), now) then
    born = old[1]
    -- A delayed writer must not rewind the idle window.
    if less(accessed, old[2]) then accessed = old[2] end
end
redis.call('HSET', KEYS[1], 'fragment', ARGV[4], 'kind', ARGV[5],
    'created_at', born, 'last_accessed', accessed, 'ttl', ttl, 'max_life', life)
if ARGV[6] == '1' then redis.call('HSET', KEYS[1], 'request_id', ARGV[7]) end
local grace = ttl
if grace == '0' then grace = '1' end
local until_expiry = add(sub(deadline(born, accessed, ttl, life), now), grace)
if less(until_expiry, '3153600000') then
    redis.call('PEXPIRE', KEYS[1], tonumber(until_expiry) * 1000)
else
    redis.call('PERSIST', KEYS[1])
end
return born
"#;

// Compare and mutate on the server: another proxy can refresh or delete a
// fragment between our read and this write. Never delete a refreshed entry
// or recreate a deleted hash containing only last_accessed.
const MUTATE_IF_UNCHANGED: &str = r#"
local fields = redis.call('HMGET', KEYS[1], 'created_at', 'last_accessed', 'ttl', 'max_life')
for i = 1, 4 do
    if fields[i] ~= ARGV[i] then return 0 end
end
if ARGV[5] == 'delete' then return redis.call('DEL', KEYS[1]) end
redis.call('HSET', KEYS[1], 'last_accessed', ARGV[6])
if ARGV[7] == 'persist' then
    redis.call('PERSIST', KEYS[1])
else
    redis.call('PEXPIRE', KEYS[1], ARGV[7])
end
return 1
"#;

fn conditional_mutation(key: &str, timing: Timing, action: &str) -> Command {
    redis_cmd![
        "EVAL",
        MUTATE_IF_UNCHANGED,
        "1",
        key,
        timing.created_at.to_string(),
        timing.last_accessed.to_string(),
        timing.ttl.to_string(),
        timing.max_life.to_string(),
        action
    ]
}

/// The timing fields of one entry, as `HMGET` returns them.
#[derive(Clone, Copy, Debug)]
struct Timing {
    created_at: u64,
    last_accessed: u64,
    ttl: u64,
    max_life: u64,
}

impl Timing {
    fn from_reply(reply: Reply) -> Option<Timing> {
        let values: Vec<Option<u64>> = reply
            .array()
            .iter()
            .map(|v| v.bulk_string().and_then(|n| n.parse().ok()))
            .collect();
        Some(Timing {
            created_at: (*values.first()?)?,
            last_accessed: (*values.get(1)?)?,
            ttl: (*values.get(2)?)?,
            max_life: (*values.get(3)?)?,
        })
    }

    fn record(self) -> RefRecord {
        RefRecord {
            // Only the timing fields are read by `expires_at`/`is_expired`.
            id: kompressor_engine::refs::ref_for(""),
            fragment: String::new(),
            kind: String::new(),
            request_id: None,
            created_at: self.created_at,
            last_accessed: self.last_accessed,
            ttl_seconds: self.ttl,
        }
    }

    fn expired(self, now: u64) -> bool {
        is_expired(&self.record(), self.max_life, now)
    }
}

/// A [`RefStore`] in Redis.
pub struct RedisRefStore {
    options: RedisOptions,
    pool: Mutex<Pool>,
    available: Condvar,
}

#[derive(Default)]
struct Pool {
    idle: Vec<Connection>,
    // Includes connections being established, so simultaneous callers cannot
    // all observe room and open a socket beyond the cap.
    total: usize,
}

struct Lease<'a> {
    store: &'a RedisRefStore,
    connection: Option<Connection>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut pool = self.store.pool.lock();
        if let Some(connection) = self.connection.take() {
            pool.idle.push(connection);
        } else {
            pool.total -= 1;
        }
        self.store.available.notify_one();
    }
}

impl std::fmt::Debug for RedisRefStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisRefStore")
            .field("target", &self.options.target)
            .field("prefix", &self.options.prefix)
            .finish()
    }
}

/// The relative expiry, in milliseconds, that lets Redis drop an entry one
/// idle window after it is dead; `None` for one that never dies.
fn redis_expiry_ms(timing: Timing, now: u64) -> Option<u64> {
    let deadline = expires_at(&timing.record(), timing.max_life);
    let until = deadline
        .saturating_sub(now)
        .saturating_add(timing.ttl.max(1));
    // Past about a century there is nothing to schedule; and Redis refuses an
    // expiry whose absolute time overflows.
    (until < 3_153_600_000).then(|| until.saturating_mul(1000))
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

impl RedisRefStore {
    /// Connect once, so a wrong address fails at startup rather than on the
    /// first compression.
    pub fn open(options: RedisOptions) -> Result<RedisRefStore, StoreError> {
        let first = Connection::open(&options.target, options.timeout)?;
        let store = RedisRefStore {
            options,
            pool: Mutex::new(Pool {
                idle: vec![first],
                total: 1,
            }),
            available: Condvar::new(),
        };
        store.checked(&[redis_cmd!["PING"]])?;
        Ok(store)
    }

    fn key(&self, tenant: &Tenant, id: &RefId) -> String {
        format!(
            "{}:r:{}:{}",
            self.options.prefix,
            hex(tenant.as_str()),
            id.body()
        )
    }

    fn pattern(&self, tenant: Option<&Tenant>) -> String {
        match tenant {
            Some(tenant) => format!("{}:r:{}:*", self.options.prefix, hex(tenant.as_str())),
            None => format!("{}:r:*", self.options.prefix),
        }
    }

    fn checkout(&self) -> Result<Lease<'_>, StoreError> {
        let started = Instant::now();
        let mut pool = self.pool.lock();
        loop {
            if let Some(connection) = pool.idle.pop() {
                return Ok(Lease {
                    store: self,
                    connection: Some(connection),
                });
            }
            if pool.total < self.options.pool.max(1) {
                pool.total += 1;
                return Ok(Lease {
                    store: self,
                    connection: None,
                });
            }
            let remaining = self.options.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(StoreError::Redis(
                    "timed out waiting for a Redis connection".into(),
                ));
            }
            self.available.wait_for(&mut pool, remaining);
        }
    }

    /// Retry a dead pooled connection once, without releasing its slot or
    /// opening a replacement until the old socket has been dropped.
    fn run(&self, commands: &[Command]) -> Result<Vec<Reply>, StoreError> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let mut lease = self.checkout()?;
        let from_pool = lease.connection.is_some();
        let mut connection = match lease.connection.take() {
            Some(connection) => connection,
            None => Connection::open(&self.options.target, self.options.timeout)?,
        };
        let replies = match connection.pipeline(commands) {
            Ok(replies) => replies,
            Err(error) if from_pool => {
                tracing::debug!(%error, "a pooled redis connection failed; reconnecting");
                drop(connection);
                connection = Connection::open(&self.options.target, self.options.timeout)?;
                connection.pipeline(commands)?
            }
            Err(error) => return Err(error),
        };
        lease.connection = Some(connection);
        Ok(replies)
    }

    fn checked(&self, commands: &[Command]) -> Result<Vec<Reply>, StoreError> {
        self.run(commands)?
            .into_iter()
            .map(Reply::into_result)
            .collect()
    }

    /// Visit unique keys in bounded batches. COUNT is only a Redis hint;
    /// split oversized pages too. Only the deduplication set spans the scan.
    fn scan_batches(
        &self,
        pattern: &str,
        mut visit: impl FnMut(&[String]) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut cursor = "0".to_string();
        let mut seen = HashSet::new();
        loop {
            let reply = self
                .checked(&[redis_cmd![
                    "SCAN",
                    cursor,
                    "MATCH",
                    pattern,
                    "COUNT",
                    BATCH_SIZE.to_string()
                ]])?
                .pop()
                .ok_or_else(|| StoreError::Redis("missing SCAN reply".into()))?;
            let Reply::Array(Some(parts)) = reply else {
                return Err(StoreError::Redis("invalid SCAN reply".into()));
            };
            let [next, Reply::Array(Some(keys))] = parts.as_slice() else {
                return Err(StoreError::Redis("invalid SCAN reply".into()));
            };
            cursor = next
                .bulk_string()
                .filter(|c| c.parse::<u64>().is_ok())
                .ok_or_else(|| StoreError::Redis("invalid SCAN cursor".into()))?;
            let mut batch = Vec::with_capacity(BATCH_SIZE);
            for key in keys {
                let key = key
                    .bulk_string()
                    .ok_or_else(|| StoreError::Redis("invalid SCAN key".into()))?;
                if seen.insert(key.clone()) {
                    batch.push(key);
                    if batch.len() == BATCH_SIZE {
                        visit(&batch)?;
                        batch.clear();
                    }
                }
            }
            if !batch.is_empty() {
                visit(&batch)?;
            }
            if cursor == "0" {
                return Ok(());
            }
        }
    }

    fn timings(&self, keys: &[String]) -> Result<Vec<Option<Timing>>, StoreError> {
        let commands: Vec<Command> = keys
            .iter()
            .map(|key| {
                let mut command = redis_cmd!["HMGET", key];
                command.extend(FIELDS.iter().map(|f| f.as_bytes().to_vec()));
                command
            })
            .collect();
        Ok(self
            .checked(&commands)?
            .into_iter()
            .map(Timing::from_reply)
            .collect())
    }

    /// The touch half of a read: restart the idle window and reschedule the
    /// Redis expiry.
    fn touch_commands(&self, key: &str, timing: Timing, now: u64) -> Vec<Command> {
        let now = now.max(timing.last_accessed);
        let touched = Timing {
            last_accessed: now,
            ..timing
        };
        let mut command = conditional_mutation(key, timing, "touch");
        command.push(now.to_string().into_bytes());
        command.push(
            redis_expiry_ms(touched, now)
                .map_or_else(|| "persist".to_string(), |ms| ms.to_string())
                .into_bytes(),
        );
        vec![command]
    }

    fn tally(&self, tenant: Option<&Tenant>) -> Result<StoreStats, StoreError> {
        let now = self.options.clock.now();
        let mut stats = StoreStats::default();
        self.scan_batches(&self.pattern(tenant), |keys| {
            let timings = self.timings(keys)?;
            let lengths: Vec<Command> = keys
                .iter()
                .map(|key| redis_cmd!["HSTRLEN", key, "fragment"])
                .collect();
            for (timing, length) in timings.into_iter().zip(self.checked(&lengths)?) {
                let Some(timing) = timing.filter(|t| !t.expired(now)) else {
                    continue;
                };
                stats.live_refs += 1;
                stats.live_bytes += length.int().unwrap_or(0).max(0) as u64;
                stats.oldest_created_at = Some(
                    stats
                        .oldest_created_at
                        .map_or(timing.created_at, |oldest| oldest.min(timing.created_at)),
                );
            }
            Ok(())
        })?;
        Ok(stats)
    }
}

impl RefStore for RedisRefStore {
    fn put_many(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<(), StoreError> {
        self.put_many_created(tenant, refs, meta).map(|_| ())
    }

    fn put_many_created(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<Option<Vec<u64>>, StoreError> {
        if refs.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let now = self.options.clock.now();
        let mut created = Vec::with_capacity(refs.len());
        for batch in refs.chunks(BATCH_SIZE) {
            let commands: Vec<Command> = batch
                .iter()
                .map(|(id, fragment)| {
                    redis_cmd![
                        "EVAL",
                        UPSERT,
                        "1",
                        self.key(tenant, id),
                        now.to_string(),
                        meta.ttl_seconds.to_string(),
                        meta.max_lifetime_seconds.to_string(),
                        fragment,
                        meta.kind.as_str(),
                        if meta.request_id.is_some() { "1" } else { "0" },
                        meta.request_id.as_deref().unwrap_or("")
                    ]
                })
                .collect();
            for reply in self.checked(&commands)? {
                created.push(
                    reply
                        .bulk_string()
                        .and_then(|s| s.parse::<u64>().ok())
                        .ok_or_else(|| {
                            StoreError::Redis("invalid upsert creation timestamp".into())
                        })?,
                );
            }
        }
        Ok(Some(created))
    }

    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError> {
        let now = self.options.clock.now();
        let key = self.key(tenant, id);
        let reply = self
            .checked(&[redis_cmd!["HGETALL", key]])?
            .pop()
            .unwrap_or(Reply::Array(None));
        let pairs = reply.array();
        if pairs.is_empty() {
            return Ok(Lookup::Miss);
        }
        let mut fields = std::collections::HashMap::new();
        for pair in pairs.chunks(2) {
            if let [name, value] = pair
                && let (Some(name), Reply::Bulk(Some(value))) = (name.bulk_string(), value)
            {
                fields.insert(name, value.clone());
            }
        }
        let number = |name: &str| -> Result<u64, StoreError> {
            fields
                .get(name)
                .and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())
                .ok_or(StoreError::CorruptRow {
                    id: id.to_string(),
                    detail: "a timing field is missing or not a number",
                })
        };
        let timing = Timing {
            created_at: number("created_at")?,
            last_accessed: number("last_accessed")?,
            ttl: number("ttl")?,
            max_life: number("max_life")?,
        };
        if timing.expired(now) {
            let expired_at = expires_at(&timing.record(), timing.max_life);
            self.checked(&[conditional_mutation(&key, timing, "delete")])?;
            return Ok(Lookup::Expired { expired_at });
        }
        let fragment = fields
            .remove("fragment")
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| StoreError::CorruptRow {
                id: id.to_string(),
                detail: "the fragment is not UTF-8",
            })?
            .ok_or(StoreError::CorruptRow {
                id: id.to_string(),
                detail: "the fragment is missing",
            })?;
        let text = |name: &str| {
            fields
                .get(name)
                .map(|v| String::from_utf8_lossy(v).into_owned())
        };
        let mut last_accessed = timing.last_accessed;
        if touch {
            self.checked(&self.touch_commands(&key, timing, now))?;
            last_accessed = now;
        }
        Ok(Lookup::Hit(RefRecord {
            id: *id,
            fragment,
            kind: text("kind").unwrap_or_default(),
            request_id: text("request_id"),
            created_at: timing.created_at,
            last_accessed,
            ttl_seconds: timing.ttl,
        }))
    }

    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let now = self.options.clock.now();
        let keys: Vec<String> = ids.iter().map(|id| self.key(tenant, id)).collect();
        let timings = self.timings(&keys)?;
        let mut commands = Vec::new();
        for (key, timing) in keys.iter().zip(timings) {
            if let Some(timing) = timing.filter(|t| !t.expired(now)) {
                commands.extend(self.touch_commands(key, timing, now));
            }
        }
        Ok(self
            .checked(&commands)?
            .iter()
            .filter_map(Reply::int)
            .sum::<i64>()
            .max(0) as usize)
    }

    fn purge_expired(&self, now: u64) -> Result<u64, StoreError> {
        let mut removed = 0;
        self.scan_batches(&self.pattern(None), |keys| {
            let timings = self.timings(keys)?;
            let dead: Vec<Command> = keys
                .iter()
                .zip(timings)
                .filter_map(|(key, timing)| {
                    timing
                        .filter(|t| t.expired(now))
                        .map(|timing| conditional_mutation(key, timing, "delete"))
                })
                .collect();
            removed += self
                .checked(&dead)?
                .iter()
                .filter_map(Reply::int)
                .sum::<i64>()
                .max(0) as u64;
            Ok(())
        })?;
        Ok(removed)
    }

    fn ping(&self) -> Result<(), StoreError> {
        self.checked(&[redis_cmd!["PING"]]).map(|_| ())
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        self.tally(None)
    }

    fn stats_for(&self, tenant: &Tenant) -> Result<StoreStats, StoreError> {
        self.tally(Some(tenant))
    }
}

/// An in-process stand-in for a Redis server: the commands this store sends,
/// over a real socket, so the client and the store are tested together.
/// Expiries are accepted and recorded, never applied — the store evaluates its
/// own deadlines, which is what the conformance suite checks.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::net::TcpListener;
    use std::sync::Arc;

    type Db = Arc<Mutex<BTreeMap<Vec<u8>, HashMap<Vec<u8>, Vec<u8>>>>>;
    type ScanPages = Arc<Mutex<Option<Vec<Vec<Vec<u8>>>>>>;

    pub(crate) struct FakeRedis {
        pub(crate) url: String,
        pub(crate) data: Db,
        pub(crate) scan_pages: ScanPages,
        pub(crate) expiries: Arc<Mutex<HashMap<Vec<u8>, i64>>>,
    }

    impl FakeRedis {
        pub(crate) fn start(password: Option<&str>) -> FakeRedis {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().unwrap().port();
            let data: Db = Arc::default();
            let expiries: Arc<Mutex<HashMap<Vec<u8>, i64>>> = Arc::default();
            let scan_pages: ScanPages = Arc::default();
            let (d, e, pages) = (data.clone(), expiries.clone(), scan_pages.clone());
            let password = password.map(str::to_string);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (d, e, pages, password) =
                        (d.clone(), e.clone(), pages.clone(), password.clone());
                    std::thread::spawn(move || serve(stream, d, e, pages, password));
                }
            });
            let url = format!("redis://127.0.0.1:{port}/0");
            FakeRedis {
                url,
                data,
                scan_pages,
                expiries,
            }
        }
    }

    fn bulk(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }

    fn glob(pattern: &[u8], key: &[u8]) -> bool {
        match pattern.iter().position(|&c| c == b'*') {
            None => pattern == key,
            Some(star) => {
                key.starts_with(&pattern[..star])
                    && key.ends_with(&pattern[star + 1..])
                    && key.len() >= pattern.len() - 1
            }
        }
    }

    fn serve(
        stream: TcpStream,
        data: Db,
        expiries: Arc<Mutex<HashMap<Vec<u8>, i64>>>,
        scan_pages: ScanPages,
        password: Option<String>,
    ) {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut authed = password.is_none();
        loop {
            let Ok(Reply::Array(Some(args))) = read_reply(&mut reader) else {
                return;
            };
            let args: Vec<Vec<u8>> = args
                .into_iter()
                .map(|a| match a {
                    Reply::Bulk(Some(b)) => b,
                    _ => Vec::new(),
                })
                .collect();
            let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
            let mut out = Vec::new();
            if !authed && name != "AUTH" {
                out.extend_from_slice(b"-NOAUTH Authentication required.\r\n");
                let _ = writer.write_all(&out);
                continue;
            }
            let mut db = data.lock();
            match name.as_str() {
                "AUTH" => {
                    let given = args.last().map(|p| String::from_utf8_lossy(p).into_owned());
                    if given == password {
                        authed = true;
                        out.extend_from_slice(b"+OK\r\n");
                    } else {
                        out.extend_from_slice(b"-WRONGPASS invalid password\r\n");
                    }
                }
                "PING" => out.extend_from_slice(b"+PONG\r\n"),
                "SELECT" => out.extend_from_slice(b"+OK\r\n"),
                "EVAL" if args[1] == UPSERT.as_bytes() => {
                    let num = |i: usize| String::from_utf8_lossy(&args[i]).parse::<u64>().unwrap();
                    let now = num(4);
                    let key = &args[3];
                    let hash = db.entry(key.clone()).or_default();
                    let old = (|| {
                        Some(Timing {
                            created_at: std::str::from_utf8(hash.get(b"created_at".as_slice())?)
                                .ok()?
                                .parse()
                                .ok()?,
                            last_accessed: std::str::from_utf8(
                                hash.get(b"last_accessed".as_slice())?,
                            )
                            .ok()?
                            .parse()
                            .ok()?,
                            ttl: std::str::from_utf8(hash.get(b"ttl".as_slice())?)
                                .ok()?
                                .parse()
                                .ok()?,
                            max_life: std::str::from_utf8(hash.get(b"max_life".as_slice())?)
                                .ok()?
                                .parse()
                                .ok()?,
                        })
                    })()
                    .filter(|timing| !timing.expired(now));
                    let timing = Timing {
                        created_at: old.map_or(now, |t| t.created_at),
                        last_accessed: old.map_or(now, |t| t.last_accessed.max(now)),
                        ttl: num(5),
                        max_life: num(6),
                    };
                    for (field, value) in [
                        ("created_at", timing.created_at.to_string().into_bytes()),
                        (
                            "last_accessed",
                            timing.last_accessed.to_string().into_bytes(),
                        ),
                        ("ttl", args[5].clone()),
                        ("max_life", args[6].clone()),
                        ("fragment", args[7].clone()),
                        ("kind", args[8].clone()),
                    ] {
                        hash.insert(field.as_bytes().to_vec(), value);
                    }
                    if args[9] == b"1" {
                        hash.insert(b"request_id".to_vec(), args[10].clone());
                    }
                    match redis_expiry_ms(timing, now) {
                        Some(ms) => {
                            expiries.lock().insert(key.clone(), ms as i64);
                        }
                        None => {
                            expiries.lock().remove(key);
                        }
                    }
                    bulk(&mut out, timing.created_at.to_string().as_bytes());
                }
                "EVAL" => {
                    assert_eq!(args[1], MUTATE_IF_UNCHANGED.as_bytes());
                    let key = &args[3];
                    let unchanged = db.get(key).is_some_and(|hash| {
                        FIELDS
                            .iter()
                            .enumerate()
                            .all(|(i, field)| hash.get(field.as_bytes()) == Some(&args[4 + i]))
                    });
                    if unchanged {
                        if args[8] == b"delete" {
                            db.remove(key);
                        } else {
                            db.get_mut(key)
                                .unwrap()
                                .insert(b"last_accessed".to_vec(), args[9].clone());
                            if args[10] == b"persist" {
                                expiries.lock().remove(key);
                            } else {
                                expiries.lock().insert(
                                    key.clone(),
                                    String::from_utf8_lossy(&args[10]).parse().unwrap(),
                                );
                            }
                        }
                    }
                    out.extend_from_slice(format!(":{}\r\n", i32::from(unchanged)).as_bytes());
                }
                "HSET" => {
                    let hash = db.entry(args[1].clone()).or_default();
                    let mut added = 0;
                    for pair in args[2..].chunks(2) {
                        if hash.insert(pair[0].clone(), pair[1].clone()).is_none() {
                            added += 1;
                        }
                    }
                    out.extend_from_slice(format!(":{added}\r\n").as_bytes());
                }
                "HMGET" => {
                    let hash = db.get(&args[1]);
                    out.extend_from_slice(format!("*{}\r\n", args.len() - 2).as_bytes());
                    for field in &args[2..] {
                        match hash.and_then(|h| h.get(field)) {
                            Some(value) => bulk(&mut out, value),
                            None => out.extend_from_slice(b"$-1\r\n"),
                        }
                    }
                }
                "HGETALL" => match db.get(&args[1]) {
                    Some(hash) => {
                        out.extend_from_slice(format!("*{}\r\n", hash.len() * 2).as_bytes());
                        for (k, v) in hash {
                            bulk(&mut out, k);
                            bulk(&mut out, v);
                        }
                    }
                    None => out.extend_from_slice(b"*0\r\n"),
                },
                "HSTRLEN" => {
                    let len = db
                        .get(&args[1])
                        .and_then(|h| h.get(&args[2]))
                        .map_or(0, Vec::len);
                    out.extend_from_slice(format!(":{len}\r\n").as_bytes());
                }
                "DEL" => {
                    let removed = args[1..].iter().filter(|k| db.remove(*k).is_some()).count();
                    out.extend_from_slice(format!(":{removed}\r\n").as_bytes());
                }
                "PEXPIRE" => {
                    let ms: i64 = String::from_utf8_lossy(&args[2]).parse().unwrap_or(-1);
                    let exists = db.contains_key(&args[1]);
                    if exists {
                        expiries.lock().insert(args[1].clone(), ms);
                    }
                    out.extend_from_slice(format!(":{}\r\n", i32::from(exists)).as_bytes());
                }
                "PERSIST" => {
                    expiries.lock().remove(&args[1]);
                    out.extend_from_slice(b":1\r\n");
                }
                "SCAN" => {
                    let pattern = args
                        .iter()
                        .position(|a| a.eq_ignore_ascii_case(b"MATCH"))
                        .map(|i| args[i + 1].clone())
                        .unwrap_or_else(|| b"*".to_vec());
                    let pages = scan_pages.lock();
                    let (next, keys) = if let Some(pages) = pages.as_ref() {
                        let cursor: usize = String::from_utf8_lossy(&args[1]).parse().unwrap();
                        (
                            if cursor + 1 < pages.len() {
                                cursor + 1
                            } else {
                                0
                            },
                            pages[cursor].clone(),
                        )
                    } else {
                        (
                            0,
                            db.keys().filter(|k| glob(&pattern, k)).cloned().collect(),
                        )
                    };
                    out.extend_from_slice(b"*2\r\n");
                    bulk(&mut out, next.to_string().as_bytes());
                    out.extend_from_slice(format!("*{}\r\n", keys.len()).as_bytes());
                    for key in keys {
                        bulk(&mut out, &key);
                    }
                }
                other => {
                    out.extend_from_slice(format!("-ERR unknown command '{other}'\r\n").as_bytes())
                }
            }
            drop(db);
            if writer.write_all(&out).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeRedis;
    use super::*;
    use crate::clock::ManualClock;
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;

    #[test]
    fn scans_deduplicate_across_pages_and_split_oversized_pages() {
        let fake = FakeRedis::start(None);
        let clock = ManualClock::at(1_000);
        let store = store_on(&fake, &clock);
        let tenant = Tenant::new("scan");
        let refs: Vec<_> = (0..BATCH_SIZE * 2 + 17)
            .map(|i| {
                let text = format!("fragment-{i}");
                (ref_for(&text), text)
            })
            .collect();
        store
            .put_many(&tenant, &refs, &PutMeta::new(Kind::Log).with_ttl(10))
            .unwrap();
        let keys: Vec<_> = refs
            .iter()
            .map(|(id, _)| store.key(&tenant, id).into_bytes())
            .collect();
        // Redis COUNT is a hint; duplicates and empty intermediate pages are legal.
        *fake.scan_pages.lock() = Some(vec![keys.clone(), vec![], keys.clone()]);
        let mut batches = Vec::new();
        store
            .scan_batches(&store.pattern(None), |keys| {
                assert!(!keys.is_empty() && keys.len() <= BATCH_SIZE);
                batches.push(keys.len());
                Ok(())
            })
            .unwrap();
        assert_eq!(batches, [BATCH_SIZE, BATCH_SIZE, 17]);
        let stats = store.stats_for(&tenant).unwrap();
        assert_eq!(stats.live_refs, refs.len() as u64);
        assert_eq!(
            stats.live_bytes,
            refs.iter().map(|(_, text)| text.len() as u64).sum::<u64>()
        );
        assert_eq!(store.purge_expired(1_011).unwrap(), refs.len() as u64);
        assert!(fake.data.lock().is_empty());
    }

    #[test]
    fn concurrent_upserts_preserve_one_birth_and_the_latest_access() {
        let fake = FakeRedis::start(None);
        check_concurrent_upserts(RedisOptions::new(&fake.url).unwrap());
    }

    fn check_concurrent_upserts(options: RedisOptions) {
        use std::sync::Barrier;
        let barrier = Barrier::new(16);
        let tenant = Tenant::new("concurrent");
        let id = ref_for("simultaneous fragment");
        let births = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|i| {
                    let mut options = options.clone();
                    options.clock = ManualClock::at(1_000 + i).clock();
                    let (barrier, tenant) = (&barrier, &tenant);
                    scope.spawn(move || {
                        let store = RedisRefStore::open(options).unwrap();
                        barrier.wait();
                        store
                            .put_many_created(
                                tenant,
                                &[(id, "simultaneous fragment".into())],
                                &PutMeta::new(Kind::Log).with_ttl(60),
                            )
                            .unwrap()
                            .unwrap()[0]
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(births.iter().all(|born| *born == births[0]), "{births:?}");
        let store = RedisRefStore::open(options).unwrap();
        let key = store.key(&tenant, &id);
        let timing = store.timings(std::slice::from_ref(&key)).unwrap()[0].unwrap();
        assert_eq!(timing.created_at, births[0]);
        assert_eq!(
            timing.last_accessed, 1_015,
            "a delayed writer must not rewind access"
        );
        store.checked(&[redis_cmd!["DEL", key]]).unwrap();
    }

    #[test]
    fn upserts_keep_u64_timestamps_exact() {
        let fake = FakeRedis::start(None);
        check_large_timestamps(RedisOptions::new(&fake.url).unwrap());
    }

    fn check_large_timestamps(mut options: RedisOptions) {
        let clock = ManualClock::at(0);
        options.clock = clock.clock();
        let store = RedisRefStore::open(options).unwrap();
        let tenant = Tenant::new("large-time");
        let id = ref_for("large timestamp");
        for start in [9_007_199_254_740_993, u64::MAX - 2] {
            clock.set(start);
            let meta = PutMeta::new(Kind::Log)
                .with_ttl(1)
                .with_max_lifetime(u64::MAX);
            let refs = [(id, "large timestamp".into())];
            assert_eq!(
                store.put_many_created(&tenant, &refs, &meta).unwrap(),
                Some(vec![start])
            );
            clock.set(start + 1);
            assert_eq!(
                store.put_many_created(&tenant, &refs, &meta).unwrap(),
                Some(vec![start])
            );
            // Still at the deadline, so the original birth must survive.
            clock.set(start + 2);
            assert_eq!(
                store.put_many_created(&tenant, &refs, &meta).unwrap(),
                Some(vec![start])
            );
            store
                .checked(&[redis_cmd!["DEL", store.key(&tenant, &id)]])
                .unwrap();
            clock.set(start);
            let meta = PutMeta::new(Kind::Log).with_ttl(0).with_max_lifetime(0);
            store.put_many(&tenant, &refs, &meta).unwrap();
            clock.set(start + 1);
            assert_eq!(
                store.put_many_created(&tenant, &refs, &meta).unwrap(),
                Some(vec![start + 1])
            );
            store
                .checked(&[redis_cmd!["DEL", store.key(&tenant, &id)]])
                .unwrap();
        }
    }

    #[test]
    fn a_full_pool_times_out_and_wakes_waiters_when_returned() {
        let fake = FakeRedis::start(None);
        let mut options = RedisOptions::new(&fake.url).unwrap();
        options.pool = 1;
        options.timeout = Duration::from_millis(100);
        let store = RedisRefStore::open(options).unwrap();
        let lease = store.checkout().unwrap();
        assert!(
            matches!(store.ping(), Err(StoreError::Redis(message)) if message.contains("waiting"))
        );
        assert_eq!(store.pool.lock().total, 1);
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let store = &store;
            scope.spawn(move || {
                tx.send(store.ping()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            drop(lease);
            rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        });
        assert_eq!(store.pool.lock().total, 1);
        assert_eq!(store.pool.lock().idle.len(), 1);
    }

    #[test]
    fn failed_reconnects_release_the_connection_slot() {
        let fake = FakeRedis::start(None);
        let mut options = RedisOptions::new(&fake.url).unwrap();
        options.pool = 1;
        let mut store = RedisRefStore::open(options).unwrap();
        let target = store.options.target.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        store.options.target.port = listener.local_addr().unwrap().port();
        drop(listener);
        for connection in &store.pool.lock().idle {
            connection
                .writer
                .shutdown(std::net::Shutdown::Both)
                .unwrap();
        }
        assert!(store.ping().is_err());
        assert_eq!(store.pool.lock().total, 0);
        // Failure opening a brand new connection releases the reservation too.
        assert!(store.ping().is_err());
        assert_eq!(store.pool.lock().total, 0);
        store.options.target = target;
        store.ping().unwrap();
        assert_eq!(store.pool.lock().total, 1);
    }

    #[test]
    fn stale_cleanup_and_touch_cannot_damage_a_concurrently_replaced_fragment() {
        let fake = FakeRedis::start(None);
        let clock = ManualClock::at(1_000);
        let store = store_on(&fake, &clock);
        check_stale_mutations(&store, &clock);
    }

    fn check_stale_mutations(store: &RedisRefStore, clock: &ManualClock) {
        let tenant = Tenant::new("acme");
        let id = ref_for("race");
        let key = store.key(&tenant, &id);
        store
            .put_many(
                &tenant,
                &[(id, "race".into())],
                &PutMeta::new(Kind::Log).with_ttl(10),
            )
            .unwrap();
        let old = store.timings(std::slice::from_ref(&key)).unwrap()[0].unwrap();
        clock.advance(11);
        // A different request refreshes the entry after cleanup read its timing.
        store
            .put_many(
                &tenant,
                &[(id, "race".into())],
                &PutMeta::new(Kind::Log).with_ttl(60),
            )
            .unwrap();
        let deleted = store
            .checked(&[conditional_mutation(&key, old, "delete")])
            .unwrap();
        assert_eq!(deleted[0].int(), Some(0));
        assert_eq!(
            store.get(&tenant, &id, false).unwrap().fragment(),
            Some("race")
        );
        let touched = store
            .checked(&store.touch_commands(&key, old, 1_012))
            .unwrap();
        assert_eq!(touched[0].int(), Some(0));
        store.checked(&[redis_cmd!["DEL", key]]).unwrap();
        let touched = store
            .checked(&store.touch_commands(&key, old, 1_012))
            .unwrap();
        assert_eq!(touched[0].int(), Some(0));
        assert_eq!(store.get(&tenant, &id, false).unwrap(), Lookup::Miss);
    }

    #[test]
    fn malformed_percent_escape_with_unicode_does_not_panic() {
        assert_eq!(
            parse_url("redis://:%€@localhost")
                .unwrap()
                .password
                .as_deref(),
            Some("%€")
        );
    }

    #[test]
    fn urls_parse_into_host_port_credentials_and_database() {
        let t = parse_url("redis://cache.internal:6380/3").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.db), ("cache.internal", 6380, 3));
        assert_eq!((t.username, t.password), (None, None));

        let t = parse_url("redis://app:p%40ss@10.0.0.5").unwrap();
        assert_eq!(t.username.as_deref(), Some("app"));
        assert_eq!(t.password.as_deref(), Some("p@ss"));
        assert_eq!((t.port, t.db), (6379, 0));

        let t = parse_url("redis://:secret@[::1]:7000/").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 7000));
        assert_eq!(t.username, None);
        assert_eq!(t.password.as_deref(), Some("secret"));
        assert!(
            !format!("{t:?}").contains("secret"),
            "the password is not in Debug"
        );
    }

    #[test]
    fn tls_and_other_schemes_are_refused_by_name() {
        assert!(parse_url("rediss://host").unwrap_err().contains("TLS"));
        assert!(parse_url("http://host").unwrap_err().contains("scheme"));
        assert!(parse_url("redis://host/abc").is_err());
        assert!(parse_url("redis://:6379").is_err());
    }

    #[test]
    fn replies_of_every_type_are_read() {
        let wire = b"+OK\r\n-ERR no\r\n:42\r\n$3\r\nabc\r\n$-1\r\n*2\r\n:1\r\n$0\r\n\r\n*-1\r\n";
        let mut reader = BufReader::new(&wire[..]);
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Simple("OK".into()));
        assert_eq!(
            read_reply(&mut reader).unwrap(),
            Reply::Error("ERR no".into())
        );
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Int(42));
        assert_eq!(
            read_reply(&mut reader).unwrap(),
            Reply::Bulk(Some(b"abc".to_vec()))
        );
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Bulk(None));
        assert_eq!(
            read_reply(&mut reader).unwrap(),
            Reply::Array(Some(vec![Reply::Int(1), Reply::Bulk(Some(Vec::new()))]))
        );
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Array(None));
        assert!(
            read_reply(&mut reader).is_err(),
            "end of stream is an error"
        );
    }

    fn store_on(fake: &FakeRedis, clock: &ManualClock) -> RedisRefStore {
        let mut options = RedisOptions::new(&fake.url).unwrap();
        options.clock = clock.clock();
        RedisRefStore::open(options).expect("the fake answers")
    }

    #[test]
    fn two_stores_on_one_server_share_their_fragments() {
        let fake = FakeRedis::start(None);
        let clock = ManualClock::at(1_000);
        let (one, two) = (store_on(&fake, &clock), store_on(&fake, &clock));
        let tenant = Tenant::new("acme");
        let id = ref_for("shared fragment");
        one.put_many(
            &tenant,
            &[(id, "shared fragment".into())],
            &PutMeta::new(Kind::Log),
        )
        .unwrap();
        assert_eq!(
            two.get(&tenant, &id, true).unwrap().fragment(),
            Some("shared fragment"),
            "minted on one proxy, resolved on another"
        );
    }

    #[test]
    fn a_tenant_name_cannot_inject_into_a_key_or_a_scan_pattern() {
        let fake = FakeRedis::start(None);
        let clock = ManualClock::at(1_000);
        let store = store_on(&fake, &clock);
        let odd = Tenant::new("a:*?[x]");
        let plain = Tenant::new("a");
        let id = ref_for("x");
        store
            .put_many(&odd, &[(id, "x".into())], &PutMeta::new(Kind::Log))
            .unwrap();
        assert_eq!(store.get(&plain, &id, false).unwrap(), Lookup::Miss);
        assert_eq!(store.stats_for(&plain).unwrap().live_refs, 0);
        assert_eq!(store.stats_for(&odd).unwrap().live_refs, 1);
        let keys: Vec<String> = fake
            .data
            .lock()
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        assert!(
            keys.iter()
                .all(|k| !k.contains('*') && k.matches(':').count() == 3),
            "{keys:?}"
        );
    }

    #[test]
    fn every_write_schedules_a_redis_expiry_past_the_deadline() {
        let fake = FakeRedis::start(None);
        let clock = ManualClock::at(1_000);
        let store = store_on(&fake, &clock);
        let tenant = Tenant::new("acme");
        let id = ref_for("y");
        store
            .put_many(
                &tenant,
                &[(id, "y".into())],
                &PutMeta::new(Kind::Log).with_ttl(60),
            )
            .unwrap();
        let expiries = fake.expiries.lock();
        let ms = *expiries.values().next().expect("an expiry was set");
        assert_eq!(ms, (60 + 60) * 1000, "one idle window past the deadline");
    }

    #[test]
    fn a_password_is_sent_and_a_wrong_one_refused() {
        let fake = FakeRedis::start(Some("hunter2"));
        let port = fake
            .url
            .rsplit(':')
            .next()
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        assert!(
            ping(
                &format!("redis://:hunter2@127.0.0.1:{port}"),
                Duration::from_secs(1)
            )
            .is_ok()
        );
        let wrong = ping(
            &format!("redis://:nope@127.0.0.1:{port}"),
            Duration::from_secs(1),
        );
        assert!(matches!(wrong, Err(StoreError::Redis(m)) if m.contains("WRONGPASS")));
    }

    #[test]
    fn an_unreachable_server_is_an_error_not_a_hang() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let options = RedisOptions::new(&format!("redis://127.0.0.1:{port}")).unwrap();
        assert!(matches!(
            RedisRefStore::open(options),
            Err(StoreError::Redis(_))
        ));
    }

    /// Against a real server when `KOMPRESSOR_TEST_REDIS` names one
    /// (`redis://127.0.0.1:6379/15`); a no-op otherwise. Uses a prefix of its
    /// own and deletes what it wrote.
    #[test]
    fn against_a_real_server_when_one_is_named() {
        let Ok(url) = std::env::var("KOMPRESSOR_TEST_REDIS") else {
            return;
        };
        let clock = ManualClock::at(1_000);
        let mut options = RedisOptions::new(&url).expect("a valid url");
        options.prefix = format!("kompressor-test-{}", std::process::id());
        options.clock = clock.clock();
        check_concurrent_upserts(options.clone());
        check_large_timestamps(options.clone());
        let store = RedisRefStore::open(options).expect("the server answers");
        let tenant = Tenant::new("acme");
        let id = ref_for("real redis fragment");
        store
            .put_many(
                &tenant,
                &[(id, "real redis fragment".into())],
                &PutMeta::new(Kind::Log).with_ttl(10),
            )
            .unwrap();
        assert_eq!(
            store.get(&tenant, &id, true).unwrap().fragment(),
            Some("real redis fragment")
        );
        clock.advance(11);
        assert!(matches!(
            store.get(&tenant, &id, false).unwrap(),
            Lookup::Expired { .. }
        ));
        assert_eq!(store.get(&tenant, &id, false).unwrap(), Lookup::Miss);
        check_stale_mutations(&store, &clock);
    }

    #[test]
    fn a_dead_pooled_connection_is_replaced() {
        let fake = FakeRedis::start(None);
        let clock = ManualClock::at(1_000);
        let store = store_on(&fake, &clock);
        // Kill the pooled connection's socket under it.
        for connection in store.pool.lock().idle.iter() {
            let _ = connection.writer.shutdown(std::net::Shutdown::Both);
        }
        assert!(store.ping().is_ok());
    }
}
