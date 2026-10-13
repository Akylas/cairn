//! Blocking HTTP GETs, byte ranges included, for use inside the synchronous render loop.
//!
//! The terrain renderer samples a pixel at a time and is wholly synchronous, while the rest of
//! this crate is async. A remote tile source has to be read from inside that loop, where there is
//! nothing to await with - and `Runtime::block_on` panics when it is called from a thread that is
//! already inside a runtime, which the render loop is: `cairn terrain` is an `async fn`. So the
//! requests are handed to a thread that owns a runtime of its own, and the caller blocks on a
//! channel instead.
//!
//! Failures are retried here rather than reported upwards, because the sampling path has no error
//! channel: a source that cannot answer looks exactly like a source with no data there, and a
//! transient 503 would otherwise punch a silent hole in the archive. What retrying cannot fix is
//! counted, so the render can say so at the end.
//!
//! A single request costs a round trip of a few hundred milliseconds against a public tile
//! service, and a worker that asks for one tile at a time spends nearly all of its life waiting
//! for it. [`Fetcher::get_many`] therefore takes a whole list and keeps `concurrency` of them in
//! flight, which is what the prefetch ahead of the render uses.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

/// Sent with every request, so an operator of a public tile service can tell who is asking.
const USER_AGENT: &str = concat!("cairn/", env!("CARGO_PKG_VERSION"), " (terrain builder)");

/// Attempts per request, including the first.
const ATTEMPTS: u32 = 4;

/// A connection that cannot be made in this long is not going to be.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The whole exchange, body included. Without a ceiling one stalled socket parks the worker that
/// asked for it - and with it the batch it belongs to - until the operating system gives up,
/// which is minutes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest a server's `Retry-After` is honoured for. A public endpoint that asks for more
/// than this is better waited out by a rerun than by a build that looks hung.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// What a tiled source did, counted with atomics so a progress line can read it while the
/// workers are in the middle of a render - the sources themselves are behind mutexes that stay
/// locked for a whole tile.
#[derive(Debug, Default)]
pub struct Counters {
    /// Requests answered, whatever the answer was.
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    /// Requests that failed every attempt. Each one is a tile missing from the output.
    pub failures: AtomicU64,
    /// Requests the server answered with "there is nothing here". The sea, mostly.
    pub not_found: AtomicU64,
    /// Extra attempts after a first one failed.
    pub retries: AtomicU64,
    /// Sum of the time every request took, retries included, in milliseconds.
    pub fetch_ms: AtomicU64,
    /// Requests started and not yet answered.
    pub in_flight: AtomicU64,
    /// Tiles decoded into elevation grids.
    pub decoded: AtomicU64,
    /// Tiles read back from the disk cache.
    pub from_disk: AtomicU64,
    /// Tiles that could not be read or decoded, for any reason.
    pub unreadable: AtomicU64,
}

impl Counters {
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// One request: a URL and, for an archive, the inclusive byte range wanted from it.
pub type Request = (String, Option<(u64, u64)>);

type Reply = Vec<Result<Option<Vec<u8>>>>;

struct Job {
    requests: Vec<Request>,
    concurrency: usize,
    reply: Sender<Reply>,
}

/// A handle onto the fetch thread. Dropping every handle stops it.
///
/// The thread is started by the first request rather than by [`Fetcher::new`]: every render
/// worker opens its own copy of every source, and eighteen threads and connection pools that
/// never fetch anything - because the prefetch already did - are not free.
pub struct Fetcher {
    jobs: OnceLock<Sender<Job>>,
    counters: Arc<Counters>,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher {
    pub fn new() -> Self {
        Self::with_counters(Arc::new(Counters::default()))
    }

    /// A fetcher that adds to counters someone else is also watching.
    pub fn with_counters(counters: Arc<Counters>) -> Self {
        Self { jobs: OnceLock::new(), counters }
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }

    fn sender(&self) -> &Sender<Job> {
        self.jobs.get_or_init(|| {
            let (jobs, inbox) = channel::<Job>();
            let counters = self.counters.clone();
            std::thread::Builder::new()
                .name("terrain-fetch".into())
                .spawn(move || serve(inbox, counters))
                .expect("spawning the terrain fetch thread");
            jobs
        })
    }

    /// Fetch a URL, or a byte range of it. `None` means the server says there is nothing there.
    pub fn get(&self, url: &str, range: Option<(u64, u64)>) -> Result<Option<Vec<u8>>> {
        self.get_many(vec![(url.to_string(), range)], 1)
            .pop()
            .unwrap_or_else(|| Err(anyhow!("the fetch thread returned nothing")))
    }

    /// Fetch several at once, `concurrency` in flight, answers in the order asked.
    pub fn get_many(&self, requests: Vec<Request>, concurrency: usize) -> Reply {
        if requests.is_empty() {
            return Vec::new();
        }
        let wanted = requests.len();
        let (reply, answer) = channel();
        let sent = self.sender().send(Job { requests, concurrency: concurrency.max(1), reply });
        let lost = |why: &str| (0..wanted).map(|_| Err(anyhow!("{why}"))).collect::<Reply>();
        if sent.is_err() {
            return lost("the fetch thread has stopped");
        }
        answer.recv().unwrap_or_else(|_| lost("the fetch thread dropped a request"))
    }

    /// `(requests, bytes, failures)` so far.
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            Counters::get(&self.counters.requests),
            Counters::get(&self.counters.bytes),
            Counters::get(&self.counters.failures),
        )
    }
}

fn serve(inbox: std::sync::mpsc::Receiver<Job>, counters: Arc<Counters>) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build();
    let (runtime, client) = match (runtime, client) {
        (Ok(r), Ok(c)) => (r, c),
        (runtime, client) => {
            // without either one nothing can be fetched; answer every request with the reason
            // rather than hanging the render on a silent channel
            let why = runtime
                .err()
                .map(|e| e.to_string())
                .or_else(|| client.err().map(|e| e.to_string()))
                .unwrap_or_default();
            for job in inbox {
                let answers = job
                    .requests
                    .iter()
                    .map(|_| Err(anyhow!("no HTTP client: {why}")))
                    .collect::<Reply>();
                let _ = job.reply.send(answers);
            }
            return;
        }
    };
    for job in inbox {
        let answers = runtime.block_on(run_job(&client, &counters, job.requests, job.concurrency));
        let _ = job.reply.send(answers);
    }
}

async fn run_job(
    client: &reqwest::Client,
    counters: &Arc<Counters>,
    requests: Vec<Request>,
    concurrency: usize,
) -> Reply {
    let limit = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    let total = requests.len();
    for (index, (url, range)) in requests.into_iter().enumerate() {
        let (client, counters, limit) = (client.clone(), counters.clone(), limit.clone());
        tasks.spawn(async move {
            // the permit is taken inside the task so that spawning never blocks, and held until
            // the answer is in
            let _permit = limit.acquire_owned().await;
            counters.in_flight.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let out = attempt(&client, &counters, &url, range).await;
            counters.in_flight.fetch_sub(1, Ordering::Relaxed);
            counters.fetch_ms.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            counters.requests.fetch_add(1, Ordering::Relaxed);
            match &out {
                Ok(Some(body)) => {
                    counters.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                }
                Ok(None) => {
                    counters.not_found.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    counters.failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            (index, out)
        });
    }
    let mut answers: Vec<Option<Result<Option<Vec<u8>>>>> = (0..total).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, out)) => answers[index] = Some(out),
            Err(e) => tracing::warn!("a terrain fetch task died: {e}"),
        }
    }
    answers
        .into_iter()
        .map(|a| a.unwrap_or_else(|| Err(anyhow!("a fetch task died before answering"))))
        .collect()
}

/// How one try went.
enum Try {
    Done(Option<Vec<u8>>),
    /// Worth another go; carries the server's own idea of how long to wait, when it gave one.
    Again(anyhow::Error, Option<Duration>),
    /// Asking again will not change the answer.
    Never(anyhow::Error),
}

async fn attempt(
    client: &reqwest::Client,
    counters: &Counters,
    url: &str,
    range: Option<(u64, u64)>,
) -> Result<Option<Vec<u8>>> {
    let mut last = None;
    let mut asked = None;
    for try_number in 0..ATTEMPTS {
        if try_number > 0 {
            counters.retries.fetch_add(1, Ordering::Relaxed);
            // a growing pause: the failures worth retrying are the busy ones. A server that
            // says how long it wants is believed over the schedule.
            let pause = asked
                .take()
                .unwrap_or_else(|| Duration::from_millis((250u64 << try_number).min(8_000)));
            tokio::time::sleep(pause).await;
        }
        match get(client, url, range).await {
            Try::Done(body) => return Ok(body),
            Try::Again(e, wait) => {
                last = Some(e);
                asked = wait;
            }
            Try::Never(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("{url} failed")))
}

async fn get(client: &reqwest::Client, url: &str, range: Option<(u64, u64)>) -> Try {
    let mut request = client.get(url);
    if let Some((start, end)) = range {
        request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    }
    let response = match request.send().await.with_context(|| format!("requesting {url}")) {
        Ok(r) => r,
        Err(e) => return Try::Again(e, None),
    };
    let status = response.status();

    // A tile that is not there is ordinary - the ocean has none - and a range past the end of an
    // archive means the same thing. Neither is worth stopping a build for.
    if status == reqwest::StatusCode::NOT_FOUND
        || status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE
    {
        return Try::Done(None);
    }
    if !status.is_success() {
        let wait = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|s| Duration::from_secs(s).min(MAX_RETRY_AFTER));
        let error = anyhow!("{url} returned {status}");
        // rate limiting and server trouble pass; a 403 or a 400 does not, and retrying it only
        // adds to the load
        return if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Try::Again(error, wait)
        } else {
            Try::Never(error)
        };
    }
    // A server that ignores `Range` answers 200 with the whole file. For a planet archive that is
    // hundreds of gigabytes down a pipe nobody asked for, so it is refused before the body is
    // read rather than after.
    if range.is_some() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Try::Never(anyhow!("{url} ignored the Range header (answered {status})"));
    }
    match response.bytes().await {
        Ok(body) => Try::Done(Some(body.to_vec())),
        Err(e) => Try::Again(anyhow!("reading {url}: {e}"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that cannot resolve must come back as an error rather than hanging the render.
    #[test]
    fn unreachable_hosts_return_an_error() {
        let fetcher = Fetcher::new();
        let out = fetcher.get("https://invalid.invalid/tile.webp", None);
        assert!(out.is_err(), "expected an error, got {out:?}");
        let (requests, _, failures) = fetcher.stats();
        assert_eq!(requests, 1);
        assert_eq!(failures, 1);
        assert_eq!(Counters::get(&fetcher.counters().retries), (ATTEMPTS - 1) as u64);
    }

    /// Answers come back in the order asked, however the requests finished, and each failure
    /// stays attached to its own request.
    #[test]
    fn many_requests_keep_their_order() {
        let fetcher = Fetcher::new();
        let asked: Vec<Request> =
            (0..5).map(|i| (format!("https://invalid.invalid/{i}.webp"), None)).collect();
        let out = fetcher.get_many(asked, 5);
        assert_eq!(out.len(), 5);
        assert!(out.iter().all(|r| r.is_err()));
        assert_eq!(fetcher.stats().0, 5);
    }

    #[test]
    fn nothing_asked_is_nothing_answered() {
        assert!(Fetcher::new().get_many(Vec::new(), 8).is_empty());
    }

    /// The thread is only started when something is asked for.
    #[test]
    fn an_idle_fetcher_has_no_thread() {
        let fetcher = Fetcher::new();
        assert!(fetcher.jobs.get().is_none());
    }
}
