//! Access to a HTTP-based crate registry.
//!
//! See [`HttpRegistry`] for details.

use crate::core::{PackageId, SourceId};
use crate::ops::{self};
use crate::sources::registry::download;
use crate::sources::registry::MaybeLock;
use crate::sources::registry::{LoadResponse, RegistryConfig, RegistryData};
use crate::util::errors::{CargoResult, HttpNotSuccessful};
use crate::util::network::Retry;
use crate::util::{auth, Config, Filesystem, IntoUrl, Progress, ProgressStyle};
use anyhow::Context;
use cargo_util::paths;
use curl::easy::{HttpVersion, List};
use curl::multi::{EasyHandle, Multi};
use log::{debug, trace};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str;
use std::task::{ready, Poll};
use std::time::Duration;
use url::Url;

// HTTP headers
const ETAG: &'static str = "etag";
const LAST_MODIFIED: &'static str = "last-modified";
const WWW_AUTHENTICATE: &'static str = "www-authenticate";
const IF_NONE_MATCH: &'static str = "if-none-match";
const IF_MODIFIED_SINCE: &'static str = "if-modified-since";

const UNKNOWN: &'static str = "Unknown";

/// A registry served by the HTTP-based registry API.
///
/// This type is primarily accessed through the [`RegistryData`] trait.
///
/// `HttpRegistry` implements the HTTP-based registry API outlined in [RFC 2789]. Read the RFC for
/// the complete protocol, but _roughly_ the implementation loads each index file (e.g.,
/// config.json or re/ge/regex) from an HTTP service rather than from a locally cloned git
/// repository. The remote service can more or less be a static file server that simply serves the
/// contents of the origin git repository.
///
/// Implemented naively, this leads to a significant amount of network traffic, as a lookup of any
/// index file would need to check with the remote backend if the index file has changed. This
/// cost is somewhat mitigated by the use of HTTP conditional fetches (`If-Modified-Since` and
/// `If-None-Match` for `ETag`s) which can be efficiently handled by HTTP/2.
///
/// [RFC 2789]: https://github.com/rust-lang/rfcs/pull/2789
pub struct HttpRegistry<'cfg> {
    index_path: Filesystem,
    cache_path: Filesystem,
    source_id: SourceId,
    config: &'cfg Config,

    /// Store the server URL without the protocol prefix (sparse+)
    url: Url,

    /// HTTP multi-handle for asynchronous/parallel requests.
    multi: Multi,

    /// Has the client requested a cache update?
    ///
    /// Only if they have do we double-check the freshness of each locally-stored index file.
    requested_update: bool,

    /// State for currently pending index downloads.
    downloads: Downloads<'cfg>,

    /// Does the config say that we can use HTTP multiplexing?
    multiplexing: bool,

    /// What paths have we already fetched since the last index update?
    ///
    /// We do not need to double-check any of these index files since we have already done so.
    fresh: HashSet<PathBuf>,

    /// Have we started to download any index files?
    fetch_started: bool,

    /// Cached registry configuration.
    registry_config: Option<RegistryConfig>,

    /// Should we include the authorization header?
    auth_required: bool,

    /// Url to get a token for the registry.
    login_url: Option<Url>,
}

/// Helper for downloading crates.
pub struct Downloads<'cfg> {
    /// When a download is started, it is added to this map. The key is a
    /// "token" (see `Download::token`). It is removed once the download is
    /// finished.
    pending: HashMap<usize, (Download<'cfg>, EasyHandle)>,
    /// Set of paths currently being downloaded.
    /// This should stay in sync with `pending`.
    pending_paths: HashSet<PathBuf>,
    /// The final result of each download.
    results: HashMap<PathBuf, CargoResult<CompletedDownload>>,
    /// The next ID to use for creating a token (see `Download::token`).
    next: usize,
    /// Progress bar.
    progress: RefCell<Option<Progress<'cfg>>>,
    /// Number of downloads that have successfully finished.
    downloads_finished: usize,
    /// Number of times the caller has requested blocking. This is used for
    /// an estimate of progress.
    blocking_calls: usize,
}

struct Download<'cfg> {
    /// The token for this download, used as the key of the `Downloads::pending` map
    /// and stored in `EasyHandle` as well.
    token: usize,

    /// The path of the package that we're downloading.
    path: PathBuf,

    /// Actual downloaded data, updated throughout the lifetime of this download.
    data: RefCell<Vec<u8>>,

    /// HTTP headers.
    header_map: RefCell<Headers>,

    /// Logic used to track retrying this download if it's a spurious failure.
    retry: Retry<'cfg>,
}

#[derive(Default)]
struct Headers {
    last_modified: Option<String>,
    etag: Option<String>,
    www_authenticate: Vec<String>,
}

enum StatusCode {
    Success,
    NotModified,
    NotFound,
    Unauthorized,
}

struct CompletedDownload {
    response_code: StatusCode,
    data: Vec<u8>,
    header_map: Headers,
}

impl<'cfg> HttpRegistry<'cfg> {
    pub fn new(
        source_id: SourceId,
        config: &'cfg Config,
        name: &str,
    ) -> CargoResult<HttpRegistry<'cfg>> {
        if !config.cli_unstable().sparse_registry {
            anyhow::bail!("usage of sparse registries requires `-Z sparse-registry`");
        }
        let url = source_id.url().as_str();
        // Ensure the url ends with a slash so we can concatenate paths.
        if !url.ends_with('/') {
            anyhow::bail!("sparse registry url must end in a slash `/`: {url}")
        }
        assert!(source_id.is_sparse());
        let url = url
            .strip_prefix("sparse+")
            .expect("sparse registry needs sparse+ prefix")
            .into_url()
            .expect("a url with the sparse+ stripped should still be valid");

        Ok(HttpRegistry {
            index_path: config.registry_index_path().join(name),
            cache_path: config.registry_cache_path().join(name),
            source_id,
            config,
            url,
            multi: Multi::new(),
            multiplexing: false,
            downloads: Downloads {
                next: 0,
                pending: HashMap::new(),
                pending_paths: HashSet::new(),
                results: HashMap::new(),
                progress: RefCell::new(Some(Progress::with_style(
                    "Fetch",
                    ProgressStyle::Indeterminate,
                    config,
                ))),
                downloads_finished: 0,
                blocking_calls: 0,
            },
            fresh: HashSet::new(),
            requested_update: false,
            fetch_started: false,
            registry_config: None,
            auth_required: false,
            login_url: None,
        })
    }

    fn handle_http_header(buf: &[u8]) -> Option<(&str, &str)> {
        if buf.is_empty() {
            return None;
        }
        let buf = std::str::from_utf8(buf).ok()?.trim_end();
        // Don't let server sneak extra lines anywhere.
        if buf.contains('\n') {
            return None;
        }
        let (tag, value) = buf.split_once(':')?;
        let value = value.trim();
        Some((tag, value))
    }

    fn start_fetch(&mut self) -> CargoResult<()> {
        if self.fetch_started {
            // We only need to run the setup code once.
            return Ok(());
        }
        self.fetch_started = true;

        // We've enabled the `http2` feature of `curl` in Cargo, so treat
        // failures here as fatal as it would indicate a build-time problem.
        self.multiplexing = self.config.http_config()?.multiplexing.unwrap_or(true);

        self.multi
            .pipelining(false, self.multiplexing)
            .with_context(|| "failed to enable multiplexing/pipelining in curl")?;

        // let's not flood the server with connections
        self.multi.set_max_host_connections(2)?;

        self.config
            .shell()
            .status("Updating", self.source_id.display_index())?;

        Ok(())
    }

    fn handle_completed_downloads(&mut self) -> CargoResult<()> {
        assert_eq!(
            self.downloads.pending.len(),
            self.downloads.pending_paths.len()
        );

        // Collect the results from the Multi handle.
        let results = {
            let mut results = Vec::new();
            let pending = &mut self.downloads.pending;
            self.multi.messages(|msg| {
                let token = msg.token().expect("failed to read token");
                let (_, handle) = &pending[&token];
                if let Some(result) = msg.result_for(handle) {
                    results.push((token, result));
                };
            });
            results
        };
        for (token, result) in results {
            let (mut download, handle) = self.downloads.pending.remove(&token).unwrap();
            let mut handle = self.multi.remove(handle)?;
            let data = download.data.take();
            let url = self.full_url(&download.path);
            let result = match download.retry.r#try(|| {
                result.with_context(|| format!("failed to download from `{}`", url))?;
                let code = handle.response_code()?;
                // Keep this list of expected status codes in sync with the codes handled in `load`
                let code = match code {
                    200 => StatusCode::Success,
                    304 => StatusCode::NotModified,
                    401 => StatusCode::Unauthorized,
                    404 | 410 | 451 => StatusCode::NotFound,
                    code => {
                        let url = handle.effective_url()?.unwrap_or(&url);
                        return Err(HttpNotSuccessful {
                            code,
                            url: url.to_owned(),
                            body: data,
                        }
                        .into());
                    }
                };
                Ok((data, code))
            }) {
                Ok(Some((data, code))) => Ok(CompletedDownload {
                    response_code: code,
                    data,
                    header_map: download.header_map.take(),
                }),
                Ok(None) => {
                    // retry the operation
                    let handle = self.multi.add(handle)?;
                    self.downloads.pending.insert(token, (download, handle));
                    continue;
                }
                Err(e) => Err(e),
            };

            assert!(self.downloads.pending_paths.remove(&download.path));
            self.downloads.results.insert(download.path, result);
            self.downloads.downloads_finished += 1;
        }

        self.downloads.tick()?;

        Ok(())
    }

    fn full_url(&self, path: &Path) -> String {
        // self.url always ends with a slash.
        format!("{}{}", self.url, path.display())
    }

    fn is_fresh(&self, path: &Path) -> bool {
        if !self.requested_update {
            trace!(
                "using local {} as user did not request update",
                path.display()
            );
            true
        } else if self.config.cli_unstable().no_index_update {
            trace!("using local {} in no_index_update mode", path.display());
            true
        } else if self.config.offline() {
            trace!("using local {} in offline mode", path.display());
            true
        } else if self.fresh.contains(path) {
            trace!("using local {} as it was already fetched", path.display());
            true
        } else {
            debug!("checking freshness of {}", path.display());
            false
        }
    }

    fn check_registry_auth_unstable(&self) -> CargoResult<()> {
        if self.auth_required && !self.config.cli_unstable().registry_auth {
            anyhow::bail!("authenticated registries require `-Z registry-auth`");
        }
        Ok(())
    }

    /// Get the cached registry configuration, if it exists.
    fn config_cached(&mut self) -> CargoResult<Option<&RegistryConfig>> {
        if self.registry_config.is_some() {
            return Ok(self.registry_config.as_ref());
        }
        let config_json_path = self
            .assert_index_locked(&self.index_path)
            .join("config.json");
        match fs::read(&config_json_path) {
            Ok(raw_data) => match serde_json::from_slice(&raw_data) {
                Ok(json) => {
                    self.registry_config = Some(json);
                }
                Err(e) => log::debug!("failed to decode cached config.json: {}", e),
            },
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    log::debug!("failed to read config.json cache: {}", e)
                }
            }
        }
        Ok(self.registry_config.as_ref())
    }

    /// Get the registry configuration.
    fn config(&mut self) -> Poll<CargoResult<&RegistryConfig>> {
        debug!("loading config");
        let index_path = self.assert_index_locked(&self.index_path);
        let config_json_path = index_path.join("config.json");
        if self.is_fresh(Path::new("config.json")) && self.config_cached()?.is_some() {
            return Poll::Ready(Ok(self.registry_config.as_ref().unwrap()));
        }

        match ready!(self.load(Path::new(""), Path::new("config.json"), None)?) {
            LoadResponse::Data {
                raw_data,
                index_version: _,
            } => {
                trace!("config loaded");
                self.registry_config = Some(serde_json::from_slice(&raw_data)?);
                if paths::create_dir_all(&config_json_path.parent().unwrap()).is_ok() {
                    if let Err(e) = fs::write(&config_json_path, &raw_data) {
                        log::debug!("failed to write config.json cache: {}", e);
                    }
                }
                Poll::Ready(Ok(self.registry_config.as_ref().unwrap()))
            }
            LoadResponse::NotFound => {
                Poll::Ready(Err(anyhow::anyhow!("config.json not found in registry")))
            }
            LoadResponse::CacheValid => Poll::Ready(Err(crate::util::internal(
                "config.json is never stored in the index cache",
            ))),
        }
    }
}

impl<'cfg> RegistryData for HttpRegistry<'cfg> {
    fn prepare(&self) -> CargoResult<()> {
        Ok(())
    }

    fn index_path(&self) -> &Filesystem {
        &self.index_path
    }

    fn assert_index_locked<'a>(&self, path: &'a Filesystem) -> &'a Path {
        self.config.assert_package_cache_locked(path)
    }

    fn is_updated(&self) -> bool {
        self.requested_update
    }

    fn load(
        &mut self,
        _root: &Path,
        path: &Path,
        index_version: Option<&str>,
    ) -> Poll<CargoResult<LoadResponse>> {
        trace!("load: {}", path.display());
        if let Some(_token) = self.downloads.pending_paths.get(path) {
            debug!("dependency is still pending: {}", path.display());
            return Poll::Pending;
        }

        if let Some(index_version) = index_version {
            trace!(
                "local cache of {} is available at version `{}`",
                path.display(),
                index_version
            );
            if self.is_fresh(path) {
                return Poll::Ready(Ok(LoadResponse::CacheValid));
            }
        } else if self.fresh.contains(path) {
            // We have no cached copy of this file, and we already downloaded it.
            debug!(
                "cache did not contain previously downloaded file {}",
                path.display()
            );
            return Poll::Ready(Ok(LoadResponse::NotFound));
        }

        if let Some(result) = self.downloads.results.remove(path) {
            let result =
                result.with_context(|| format!("download of {} failed", path.display()))?;

            assert!(
                self.fresh.insert(path.to_path_buf()),
                "downloaded the index file `{}` twice",
                path.display()
            );

            // The status handled here need to be kept in sync with the codes handled
            // in `handle_completed_downloads`
            match result.response_code {
                StatusCode::Success => {
                    let response_index_version = if let Some(etag) = result.header_map.etag {
                        format!("{}: {}", ETAG, etag)
                    } else if let Some(lm) = result.header_map.last_modified {
                        format!("{}: {}", LAST_MODIFIED, lm)
                    } else {
                        UNKNOWN.to_string()
                    };
                    trace!("index file version: {}", response_index_version);
                    return Poll::Ready(Ok(LoadResponse::Data {
                        raw_data: result.data,
                        index_version: Some(response_index_version),
                    }));
                }
                StatusCode::NotModified => {
                    // Not Modified: the data in the cache is still the latest.
                    if index_version.is_none() {
                        return Poll::Ready(Err(anyhow::anyhow!(
                            "server said not modified (HTTP 304) when no local cache exists"
                        )));
                    }
                    return Poll::Ready(Ok(LoadResponse::CacheValid));
                }
                StatusCode::NotFound => {
                    // The crate was not found or deleted from the registry.
                    return Poll::Ready(Ok(LoadResponse::NotFound));
                }
                StatusCode::Unauthorized
                    if !self.auth_required && path == Path::new("config.json") =>
                {
                    debug!("re-attempting request for config.json with authorization included.");
                    self.fresh.remove(path);
                    self.auth_required = true;

                    // Look for a `www-authenticate` header with the `Cargo` scheme.
                    for header in &result.header_map.www_authenticate {
                        for challenge in http_auth::ChallengeParser::new(header) {
                            match challenge {
                                Ok(challenge) if challenge.scheme.eq_ignore_ascii_case("Cargo") => {
                                    // Look for the `login_url` parameter.
                                    for (param, value) in challenge.params {
                                        if param.eq_ignore_ascii_case("login_url") {
                                            self.login_url = Some(value.to_unescaped().into_url()?);
                                        }
                                    }
                                }
                                Ok(challenge) => {
                                    debug!("ignoring non-Cargo challenge: {}", challenge.scheme)
                                }
                                Err(e) => debug!("failed to parse challenge: {}", e),
                            }
                        }
                    }
                }
                StatusCode::Unauthorized => {
                    let err = Err(HttpNotSuccessful {
                        code: 401,
                        body: result.data,
                        url: self.full_url(path),
                    }
                    .into());
                    if self.auth_required {
                        return Poll::Ready(err.context(auth::AuthorizationError {
                            sid: self.source_id.clone(),
                            login_url: self.login_url.clone(),
                            reason: auth::AuthorizationErrorReason::TokenRejected,
                        }));
                    } else {
                        return Poll::Ready(err);
                    }
                }
            }
        }

        if path != Path::new("config.json") {
            self.auth_required = ready!(self.config()?).auth_required;
        } else if !self.auth_required {
            // Check if there's a cached config that says auth is required.
            // This allows avoiding the initial unauthenticated request to probe.
            if let Some(config) = self.config_cached()? {
                self.auth_required = config.auth_required;
            }
        }

        // Looks like we're going to have to do a network request.
        self.start_fetch()?;

        let mut handle = ops::http_handle(self.config)?;
        let full_url = self.full_url(path);
        debug!("fetch {}", full_url);
        handle.get(true)?;
        handle.url(&full_url)?;
        handle.follow_location(true)?;

        // Enable HTTP/2 if possible.
        if self.multiplexing {
            handle.http_version(HttpVersion::V2)?;
        } else {
            handle.http_version(HttpVersion::V11)?;
        }

        // This is an option to `libcurl` which indicates that if there's a
        // bunch of parallel requests to the same host they all wait until the
        // pipelining status of the host is known. This means that we won't
        // initiate dozens of connections to crates.io, but rather only one.
        // Once the main one is opened we realized that pipelining is possible
        // and multiplexing is possible with static.crates.io. All in all this
        // reduces the number of connections done to a more manageable state.
        handle.pipewait(true)?;

        let mut headers = List::new();
        // Include a header to identify the protocol. This allows the server to
        // know that Cargo is attempting to use the sparse protocol.
        headers.append("cargo-protocol: version=1")?;
        headers.append("accept: text/plain")?;

        // If we have a cached copy of the file, include IF_NONE_MATCH or IF_MODIFIED_SINCE header.
        if let Some(index_version) = index_version {
            if let Some((key, value)) = index_version.split_once(':') {
                match key {
                    ETAG => headers.append(&format!("{}: {}", IF_NONE_MATCH, value.trim()))?,
                    LAST_MODIFIED => {
                        headers.append(&format!("{}: {}", IF_MODIFIED_SINCE, value.trim()))?
                    }
                    _ => debug!("unexpected index version: {}", index_version),
                }
            }
        }
        if self.auth_required {
            self.check_registry_auth_unstable()?;
            let authorization =
                auth::auth_token(self.config, &self.source_id, self.login_url.as_ref(), None)?;
            headers.append(&format!("Authorization: {}", authorization))?;
            trace!("including authorization for {}", full_url);
        }
        handle.http_headers(headers)?;

        // We're going to have a bunch of downloads all happening "at the same time".
        // So, we need some way to track what headers/data/responses are for which request.
        // We do that through this token. Each request (and associated response) gets one.
        let token = self.downloads.next;
        self.downloads.next += 1;
        debug!("downloading {} as {}", path.display(), token);
        assert!(
            self.downloads.pending_paths.insert(path.to_path_buf()),
            "path queued for download more than once"
        );

        // Each write should go to self.downloads.pending[&token].data.
        // Since the write function must be 'static, we access downloads through a thread-local.
        // That thread-local is set up in `block_until_ready` when it calls self.multi.perform,
        // which is what ultimately calls this method.
        handle.write_function(move |buf| {
            trace!("{} - {} bytes of data", token, buf.len());
            tls::with(|downloads| {
                if let Some(downloads) = downloads {
                    downloads.pending[&token]
                        .0
                        .data
                        .borrow_mut()
                        .extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        })?;

        // And ditto for the header function.
        handle.header_function(move |buf| {
            if let Some((tag, value)) = Self::handle_http_header(buf) {
                tls::with(|downloads| {
                    if let Some(downloads) = downloads {
                        let mut header_map = downloads.pending[&token].0.header_map.borrow_mut();
                        match tag.to_ascii_lowercase().as_str() {
                            LAST_MODIFIED => header_map.last_modified = Some(value.to_string()),
                            ETAG => header_map.etag = Some(value.to_string()),
                            WWW_AUTHENTICATE => header_map.www_authenticate.push(value.to_string()),
                            _ => {}
                        }
                    }
                });
            }

            true
        })?;

        let dl = Download {
            token,
            path: path.to_path_buf(),
            data: RefCell::new(Vec::new()),
            header_map: Default::default(),
            retry: Retry::new(self.config)?,
        };

        // Finally add the request we've lined up to the pool of requests that cURL manages.
        let mut handle = self.multi.add(handle)?;
        handle.set_token(token)?;
        self.downloads.pending.insert(dl.token, (dl, handle));

        Poll::Pending
    }

    fn config(&mut self) -> Poll<CargoResult<Option<RegistryConfig>>> {
        let cfg = ready!(self.config()?).clone();
        self.check_registry_auth_unstable()?;
        Poll::Ready(Ok(Some(cfg)))
    }

    fn invalidate_cache(&mut self) {
        // Actually updating the index is more or less a no-op for this implementation.
        // All it does is ensure that a subsequent load will double-check files with the
        // server rather than rely on a locally cached copy of the index files.
        debug!("invalidated index cache");
        self.fresh.clear();
        self.requested_update = true;
    }

    fn download(&mut self, pkg: PackageId, checksum: &str) -> CargoResult<MaybeLock> {
        let registry_config = loop {
            match self.config()? {
                Poll::Pending => self.block_until_ready()?,
                Poll::Ready(cfg) => break cfg.to_owned(),
            }
        };
        download::download(
            &self.cache_path,
            &self.config,
            pkg,
            checksum,
            registry_config,
        )
    }

    fn finish_download(
        &mut self,
        pkg: PackageId,
        checksum: &str,
        data: &[u8],
    ) -> CargoResult<File> {
        download::finish_download(&self.cache_path, &self.config, pkg, checksum, data)
    }

    fn is_crate_downloaded(&self, pkg: PackageId) -> bool {
        download::is_crate_downloaded(&self.cache_path, &self.config, pkg)
    }

    fn block_until_ready(&mut self) -> CargoResult<()> {
        trace!(
            "block_until_ready: {} transfers pending",
            self.downloads.pending.len()
        );
        self.downloads.blocking_calls += 1;

        loop {
            self.handle_completed_downloads()?;

            let remaining_in_multi = tls::set(&self.downloads, || {
                self.multi
                    .perform()
                    .with_context(|| "failed to perform http requests")
            })?;
            trace!("{} transfers remaining", remaining_in_multi);

            if remaining_in_multi == 0 {
                return Ok(());
            }

            // We have no more replies to provide the caller with,
            // so we need to wait until cURL has something new for us.
            let timeout = self
                .multi
                .get_timeout()?
                .unwrap_or_else(|| Duration::new(1, 0));
            self.multi
                .wait(&mut [], timeout)
                .with_context(|| "failed to wait on curl `Multi`")?;
        }
    }
}

impl<'cfg> Downloads<'cfg> {
    fn tick(&self) -> CargoResult<()> {
        let mut progress = self.progress.borrow_mut();
        let progress = progress.as_mut().unwrap();

        // Since the sparse protocol discovers dependencies as it goes,
        // it's not possible to get an accurate progress indication.
        //
        // As an approximation, we assume that the depth of the dependency graph
        // is fixed, and base the progress on how many times the caller has asked
        // for blocking. If there are actually additional dependencies, the progress
        // bar will get stuck. If there are fewer dependencies, it will disappear
        // early. It will never go backwards.
        //
        // The status text also contains the number of completed & pending requests, which
        // gives an better indication of forward progress.
        let approximate_tree_depth = 10;

        progress.tick(
            self.blocking_calls.min(approximate_tree_depth),
            approximate_tree_depth + 1,
            &format!(
                " {} complete; {} pending",
                self.downloads_finished,
                self.pending.len()
            ),
        )
    }
}

mod tls {
    use super::Downloads;
    use std::cell::Cell;

    thread_local!(static PTR: Cell<usize> = Cell::new(0));

    pub(crate) fn with<R>(f: impl FnOnce(Option<&Downloads<'_>>) -> R) -> R {
        let ptr = PTR.with(|p| p.get());
        if ptr == 0 {
            f(None)
        } else {
            // Safety: * `ptr` is only set by `set` below which ensures the type is correct.
            let ptr = unsafe { &*(ptr as *const Downloads<'_>) };
            f(Some(ptr))
        }
    }

    pub(crate) fn set<R>(dl: &Downloads<'_>, f: impl FnOnce() -> R) -> R {
        struct Reset<'a, T: Copy>(&'a Cell<T>, T);

        impl<'a, T: Copy> Drop for Reset<'a, T> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }

        PTR.with(|p| {
            let _reset = Reset(p, p.get());
            p.set(dl as *const Downloads<'_> as usize);
            f()
        })
    }
}
