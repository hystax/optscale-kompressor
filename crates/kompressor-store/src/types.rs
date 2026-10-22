//! The store contract: what a fragment record holds, and when it stops being
//! retrievable.

use kompressor_engine::refs::RefId;
use kompressor_engine::types::Kind;
use std::fmt;

/// The default idle window, in seconds.
pub const DEFAULT_TTL_SECONDS: u64 = 3_600;

/// How much longer than one idle window an entry may live in total.
///
/// Eight windows: long enough that an agent
/// that keeps quoting a marker keeps its fragment, short enough that a
/// conversation left open overnight cannot pin yesterday's tool output.
pub const DEFAULT_LIFETIME_MULTIPLIER: u64 = 8;

/// The default absolute lifetime, measured from `created_at`.
pub const DEFAULT_MAX_LIFETIME_SECONDS: u64 = DEFAULT_TTL_SECONDS * DEFAULT_LIFETIME_MULTIPLIER;

/// The isolation boundary. `\"\"` is the anonymous single-tenant deployment.
///
/// The tenant is part of the *key*, never a field compared after a row comes
/// back. A handle that leaks across tenants therefore addresses a row that does
/// not exist, and there is no post-read check for a later refactor to drop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tenant(pub String);

impl Tenant {
    /// The empty tenant used when the deployment has no notion of one.
    pub fn anonymous() -> Tenant {
        Tenant(String::new())
    }

    /// A named tenant.
    pub fn new(name: impl Into<String>) -> Tenant {
        Tenant(name.into())
    }

    /// The tenant name as it appears in the key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the anonymous tenant.
    pub fn is_anonymous(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for Tenant {
    fn from(name: &str) -> Tenant {
        Tenant(name.to_owned())
    }
}

impl From<String> for Tenant {
    fn from(name: String) -> Tenant {
        Tenant(name)
    }
}

impl fmt::Display for Tenant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything a write carries besides the fragments themselves.
///
/// One `PutMeta` covers a whole batch because a batch is one request's worth of
/// elisions: same tenant, same request id, same TTL policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutMeta {
    /// The detected content type of the payload the fragment came out of, kept
    /// for statistics and debugging only — nothing in the store routes on it.
    pub kind: Kind,
    /// The request that minted these handles, when the caller tracks one.
    pub request_id: Option<String>,
    /// The idle window: a successful touching read restarts it.
    pub ttl_seconds: u64,
    /// The cap on total life, from `created_at`, that bounds the sliding window.
    pub max_lifetime_seconds: u64,
    /// Optional original creation timestamp to preserve the absolute lifetime
    /// when re-storing a fragment that may have been evicted from a cache.
    /// When `None`, the fragment is treated as new.
    pub created_at: Option<u64>,
}

impl PutMeta {
    /// Default TTL policy for `kind`.
    pub fn new(kind: Kind) -> PutMeta {
        PutMeta {
            kind,
            request_id: None,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            max_lifetime_seconds: DEFAULT_MAX_LIFETIME_SECONDS,
            created_at: None,
        }
    }

    /// Attribute the write to a request.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> PutMeta {
        self.request_id = Some(request_id.into());
        self
    }

    /// Set the idle window, with the absolute cap at the default multiple of it.
    pub fn with_ttl(mut self, ttl_seconds: u64) -> PutMeta {
        self.ttl_seconds = ttl_seconds;
        self.max_lifetime_seconds = ttl_seconds.saturating_mul(DEFAULT_LIFETIME_MULTIPLIER);
        self
    }

    /// Set the absolute lifetime independently of the idle window.
    pub fn with_max_lifetime(mut self, max_lifetime_seconds: u64) -> PutMeta {
        self.max_lifetime_seconds = max_lifetime_seconds;
        self
    }

    /// Carry the original creation timestamp so a re-store does not restart
    /// the absolute lifetime (used by caches that may have dropped the entry).
    pub fn with_created_at(mut self, created_at: u64) -> PutMeta {
        self.created_at = Some(created_at);
        self
    }
}

/// One stored fragment.
///
/// `kind` is a `String` rather than a [`Kind`]: the row may have been written
/// by a build whose enum had a variant this one does not, and a read that
/// refuses to parse it would turn a retrievable fragment into an error. The
/// field is descriptive, so the unrecognised name is passed through.
#[derive(Clone, PartialEq, Eq)]
pub struct RefRecord {
    /// The content-addressed handle the marker carries.
    pub id: RefId,
    /// The original text that was elided.
    pub fragment: String,
    /// The content type of the payload it came from.
    pub kind: String,
    /// The request that minted it, if one was recorded.
    pub request_id: Option<String>,
    /// When it was first stored. Re-storing the same handle does not move this.
    pub created_at: u64,
    /// When it was last written or read with `touch`.
    pub last_accessed: u64,
    /// The idle window in force for this row.
    pub ttl_seconds: u64,
}

impl RefRecord {
    /// UTF-8 bytes of the fragment — what the byte cap and [`StoreStats`] count.
    pub fn bytes(&self) -> u64 {
        self.fragment.len() as u64
    }
}

impl fmt::Debug for RefRecord {
    /// Renders the fragment's size instead of the fragment.
    ///
    /// Fragments are tool output: an AWS response, a `.env` read, a log line
    /// carrying a bearer token. `#[derive(Debug)]` here would put those bytes
    /// into any error chain, panic message or `tracing` field that ever
    /// formatted a record — places nobody audits for secrets. Making the type
    /// incapable of printing its own body is cheaper than remembering not to.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(\"RefRecord\")
            .field(\"id\", &self.id)
            .field(\"fragment_bytes\", &self.fragment.len())
            .field(\"kind\", &self.kind)
            .field(\"request_id\", &self.request_id)
            .field(\"created_at\", &self.created_at)
            .field(\"last_accessed\", &self.last_accessed)
            .field(\"ttl_seconds\", &self.ttl_seconds)
            .finish()
    }
}

/// The outcome of a point read.
///
/// `Expired` is deliberately not folded into `Miss`. The caller answers a model
/// that asked for a marker by name, and \"we held this and let it go\" earns a
/// different reply — and a different stats row — from \"this handle was never
/// ours\", which is what a cross-tenant or hallucinated handle looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The fragment, live at the time of the read.
    Hit(RefRecord),
    /// No row for this (tenant, handle).
    Miss,
    /// A row existed and was past its window; it has been dropped.
    Expired {
        /// The instant the entry stopped being retrievable.
        expired_at: u64,
    },
}

impl Lookup {
    /// The fragment text, or `None` for a miss or an expiry.
    pub fn fragment(&self) -> Option<&str> {
        match self {
            Lookup::Hit(record) => Some(&record.fragment),
            _ => None,
        }
    }

    /// Whether the fragment was returned.
    pub fn is_hit(&self) -> bool {
        matches!(self, Lookup::Hit(_))
    }
}

/// A snapshot of what the store is holding, counting live rows only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Rows that a read would currently answer with.
    pub live_refs: u64,
    /// UTF-8 bytes of fragment held by those rows.
    pub live_bytes: u64,
    /// `created_at` of the oldest live row, or `None` when empty.
    pub oldest_created_at: Option<u64>,
}

/// Why an operation could not be carried out.
///
/// Callers treat any of these as a miss and carry on — a fragment that cannot
/// be fetched costs the model one retrieval, while a failed request costs the
/// turn — so the variants exist to be reported accurately, not retried.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The SQLite driver refused the statement.
    #[error(\"sqlite: {0}\")]
    Sqlite(#[from] rusqlite::Error),

    /// A schema migration failed; the database is left at the previous version.
    #[error(\"refs schema migration to version {version} failed: {source}\")]
    Migration {
        /// The migration that failed.
        version: u32,
        /// What the driver reported.
        #[source]
        source: rusqlite::Error,
    },

    /// A stored row cannot be turned back into a [`RefRecord`].
    ///
    /// `detail` is a fixed string and the fragment is never attached: a row is
    /// most likely unreadable *because* its bytes are unexpected, which is
    /// exactly when quoting them into a log is most dangerous.
    #[error(\"stored row for handle {id} is unreadable: {detail}\")]
    CorruptRow {
        /// The handle, as stored.
        id: String,
        /// What was wrong with it.
        detail: &'static str,
    },
}

/// When an entry stops being retrievable.
///
/// The idle window slides with every touching read; the absolute lifetime does
/// not move, so whichever comes first wins. Saturating arithmetic keeps an
/// absurd TTL (`u64::MAX`, \"never expire\") from wrapping into a timestamp in
/// the past, which would expire the entry immediately — the exact opposite of
/// what was asked for.
pub fn expires_at(record: &RefRecord, max_lifetime_seconds: u64) -> u64 {
    let idle_deadline = record.last_accessed.saturating_add(record.ttl_seconds);
    let absolute_deadline = record.created_at.saturating_add(max_lifetime_seconds);
    idle_deadline.min(absolute_deadline)
}

/// Whether `record` is past its window at `now`.
///
/// Dead means strictly past the deadline, matching the `<` in the purge
/// statement: a row can never read as `Expired` on one path while a sweep on
/// the other considers it live, and a fragment stored with a zero TTL is still
/// retrievable within the same second it was written.
pub fn is_expired(record: &RefRecord, max_lifetime_seconds: u64, now: u64) -> bool {
    expires_at(record, max_lifetime_seconds) < now
}

/// A tenant-scoped, TTL-bounded store of compression fragments.
///
/// Implementations are shared across request threads (`Send + Sync`) and take
/// `&self` throughout: a compression flushing a batch must not block the
/// retrievals landing beside it.
pub trait RefStore: Send + Sync {
    /// Idempotent upsert of one request's fragments.
    ///
    /// The handle is a digest of the fragment, so re-storing one is not a
    /// change of content — its real effect is restarting the idle window.
    /// `created_at` therefore survives the overwrite: if it did not, a
    /// re-compressed history would reset the absolute lifetime every turn and
    /// the cap would never bite.
    fn put_many(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<(), StoreError>;

    /// Point read scoped by tenant.
    ///
    /// With `touch`, a hit restarts the idle window. Pass `false` for reads
    /// that must not keep an entry alive, such as an operator inspecting the
    /// store.
    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError>;

    /// Check durable storage before advertising a newly written handle.
    /// A cache may answer `get` while its backing write is still in flight;
    /// implementations with a cache override this to consult the backing store.
    fn verify(&self, tenant: &Tenant, id: &RefId) -> Result<Lookup, StoreError> {
        self.get(tenant, id, false)
    }

    /// Restart the idle window of several entries, returning how many were
    /// found live.
    ///
    /// This is what keeps a marker that drifted into content nobody
    /// re-compresses — a frozen prefix, or the model quoting the marker back in
    /// its own turn — pointing at a fragment that is still there. The caller
    /// scans protected content for handles every request and touches them; the
    /// absolute lifetime still bounds how long that can go on. Expired entries
    /// are not resurrected, so a count below `ids.len()` is the honest answer.
    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError>;

    /// Delete every entry dead at `now`, across all tenants, returning the count.
    ///
    /// `now` is a parameter rather than a clock read so a sweep can be run
    /// against an explicit instant.
    fn purge_expired(&self, now: u64) -> Result<u64, StoreError>;

    /// Live rows and bytes.
    fn stats(&self) -> Result<StoreStats, StoreError>;

    /// The same figures for one tenant.
    ///
    /// Separate from [`Self::stats`] because the whole-store total is a
    /// deployment-level diagnostic and must not be served to a tenant as its
    /// own: `GET /v1/stats/store` authenticated the caller, scoped the tenant,
    /// and then reported every tenant's rows and bytes to it.
    fn stats_for(&self, tenant: &Tenant) -> Result<StoreStats, StoreError>;
}
