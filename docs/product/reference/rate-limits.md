# Rate limits and concurrency

This page lists the limits that protect the proxy against overload, abuse, and resource exhaustion. Every limit has a default and an environment-variable override.

## Auth rate limiter

The auth rate limiter protects SigV4 authentication and the admin login endpoints against brute-force guessing. The admin login endpoints count failures for each client IP address. The S3 API counts failures for each pair of client IP address and access key id: a key that keeps failing at an address is locked out at that address, and the other keys at the same address are not. Every one of these counts treats an IPv4 address as itself (an IPv4-mapped IPv6 address counts as its IPv4 address) and an IPv6 address as its /64 network, because one IPv6 client owns a whole /64 and can use a new address for each request. The limit on connections (see [Connection limits](#connection-limits)) counts addresses in the same way. Behind a reverse proxy every client has the address of the proxy, so a lockout of the whole address would refuse every client because of one client with a wrong secret.

| Setting | Default | Env var |
|---------|---------|---------|
| Max failures before lockout | 100 | `DGP_RATE_LIMIT_MAX_ATTEMPTS` |
| Rolling window | 300 s (5 min) | `DGP_RATE_LIMIT_WINDOW_SECS` |
| Lockout duration | 600 s (10 min) | `DGP_RATE_LIMIT_LOCKOUT_SECS` |

After a lockout expires, the failure counter resets and the IP (or the key at that IP) can authenticate again. A successful request does not reset the counter of its IP address: the counter holds the guesses at every secret that the address tried, and one success proves only one secret. The counter ends when its window ends or when the lockout ends. A successful admin sign-in (the bootstrap password, `login-as`, `recover-db`) clears the per-account counter of the account it proved.

### Per-account lockout

The admin sign-in endpoints also count failures for each account, from every IP address together. The bootstrap password login and `recover-db` count against the account `bootstrap`, and `login-as` counts against the access key id that the request names. An attacker who sends guesses from many IP addresses stays below every per-IP limit, but the per-account count still stops the guesses.

| Setting | Default | Env var |
|---------|---------|---------|
| Max failures for one account before it locks | 10 | `DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS` |
| Rolling window | 3600 s (1 h) | `DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS` |
| Lockout duration | 3600 s (1 h) | `DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS` |

A locked account still admits two kinds of caller, so that the attacker cannot lock the operator out. The first is an IP address that signed in to that account successfully within the last 30 days. The second is a direct request from the host itself: its TCP peer is a loopback address and it carries no forwarding header, for example a request through an SSH tunnel. The per-IP limit still applies to both.

Only a failed credential counts toward the lockout. On the S3 API, a failure is a request whose signature the proxy checked and refused: a wrong secret for a known access key, a signature that is too old, or an unknown or disabled access key id. The proxy checks the signature of an unknown or disabled access key id against a secret that nobody has, so the answer is `SignatureDoesNotMatch`, as for a wrong secret. The proxy counts a failed signature under the pair of client address and access key id as the client sent it, whether that id exists or not, so the delay and the lockout are the same for a known and an unknown id. The proxy keeps at most 65,536 such pairs. When the table is full, it forgets the oldest pairs that are not locked out first, and a full table never refuses a request. Some requests are refused but not counted: a request with no credentials, a malformed `Authorization` header, and an expired presigned link. These requests never reach a signature check, so they teach the sender nothing about a secret. If they counted, any anonymous client could lock out every client that shares its IP address, for example all clients behind one load balancer. On a browser form upload (a `POST` with `multipart/form-data`), only a signature that does not match counts. The access key of a form upload is in the form body, which only the form handler reads, so the form uploads from one address count together as one key. A read of a public prefix needs no credentials, so the proxy serves it to a locked-out IP too.

A locked-out admin sign-in gets a response that names the lockout and says how long it lasts. The correct password is refused too while the lockout lasts. The S3 API works differently: a lockout never refuses a request whose signature verifies. During the lockout of a key at an address, the proxy still checks the signature of each request of that key from that address. It answers only a request whose signature fails. It makes that request wait the longest progressive delay (5 seconds) before the `503 SlowDown` answer (when a wait place of its address is free, see below), and it does not count it, so a guess during a lockout is never cheaper than a guess before it. A request whose signature verifies is served as usual and is not delayed. This matters because access key ids are not secret (every presigned link carries one), so anyone who shares an address with a client can send bad signatures for the key id of that client.

A failed signature waits outside the request slots (`DGP_MAX_CONCURRENT_REQUESTS`): the proxy gives the slot back and then waits, so waiting failures never take slots from other clients. A waiting failure still uses memory, so the proxy gives each client address 32 wait places. A signed request takes one place when it arrives, and it gives the place back as soon as its signature verifies. A failed request keeps its place until its wait ends. An IPv6 address counts by its /64 network. When all 32 places of an address are taken, the next signed request of that address is not refused: the proxy checks its signature as usual. A request whose signature verifies is served at once, as always. A request whose signature fails is answered at once, with the answer that it would get anyway (`403 SignatureDoesNotMatch`, or the `503 SlowDown` of a lockout), without a wait and without a place. So the number of failures that sleep for one address never passes 32, and no signed request is refused because other requests of its address are waiting.

This design has one cost. An address that keeps more than 32 failed signatures waiting at the same time gets its further wrong guesses answered without the delay, so the delay limits the guesses of one address only up to those 32 places. The lockout of an address and access key id still starts after the configured number of failures, and it still answers every failed signature of that pair with `503` until it ends. The access key secrets that the proxy generates are too long to guess online. If you choose a bootstrap secret yourself, make it long and random.

- The admin API sign-in endpoints (`/_/api/admin/login`, `login-as`, the browser-session endpoints, `recover-db`, and a `/_/metrics` bearer token) answer `429 Too Many Requests` with a `Retry-After` header in seconds and the JSON body `{"ok": false, "error": "too_many_attempts", "message": "Too many failed sign-in attempts. Try again in 10 min.", "retry_after_secs": 600}`.
- The single sign-on pages (`/_/api/admin/oauth/authorize/<provider>` and `/_/api/admin/oauth/callback`) are pages that the browser opens, not API requests. So they answer `429 Too Many Requests` with the same `Retry-After` header and an HTML error page that says how long to wait.
- The S3 API answers `503 SlowDown` to a request whose signature fails during a lockout, because S3 clients know that code. The response has the same `Retry-After` header and the wait in the error message.

### Progressive delay

Before an IP address (on the S3 API, a key at an IP address) reaches the lockout threshold, each failed auth attempt adds an artificial delay to its own refused response. A request that succeeds is not delayed, so a client that shares an address with a failing client keeps its speed:

| Failures | Delay |
|----------|-------|
| 1 to 10 | none |
| 11 | 200 ms |
| 12 | 400 ms |
| 13 | 800 ms |
| 14 | 1.6 s |
| 15 | 3.2 s |
| 16+ | 5 s (cap) |

### IP extraction

Rate limiting requires a client IP. The proxy reads the `X-Forwarded-For` or `X-Real-IP` headers only when `DGP_TRUST_PROXY_HEADERS=true`. The default is `false`, so in a deployment that faces the internet directly, a client cannot spoof its IP address with these headers. Set `DGP_TRUST_PROXY_HEADERS=true` only behind a trusted reverse proxy (nginx, Caddy, ALB) that injects these headers.

With `DGP_TRUST_PROXY_HEADERS=true`, `DGP_TRUSTED_PROXY_CIDRS` must list the networks of the reverse proxies, and the proxy reads these headers only from a connection that comes from one of those networks. From any other connection it uses the address of the connection. From a trusted proxy, `X-Forwarded-For` wins: the proxy walks the header from right to left and takes the first address that is not in a trusted network, or the leftmost address when every address in the header is trusted. When the request has no `X-Forwarded-For` header, the proxy takes `X-Real-IP`, because nginx often sets only that header. When neither header is present, it takes the address of the connection. A trusted reverse proxy must therefore overwrite any `X-Real-IP` value that the client sends (nginx `proxy_set_header X-Real-IP $remote_addr` does), or else set `X-Forwarded-For`.

The proxy refuses to start when `DGP_TRUST_PROXY_HEADERS=true` and `DGP_TRUSTED_PROXY_CIDRS` is unset or holds no valid network. Without the list, the proxy cannot tell a header that a reverse proxy wrote from a header that the client forged. If the proxy used the first `X-Forwarded-For` address, any client could choose the address that the rate limiter locks out.

> **Failure mode behind a proxy.** If the proxy sits behind a reverse proxy and `DGP_TRUST_PROXY_HEADERS` stays `false`, every request appears to come from the IP address of the reverse proxy. On the S3 API, a client that keeps sending a wrong secret then locks out its access key for every client behind the proxy, and the clients with other keys are not affected. The admin login endpoints count by address only, so failed admin logins lock out the admin logins of every client behind the proxy. Set `DGP_TRUST_PROXY_HEADERS=true` and `DGP_TRUSTED_PROXY_CIDRS` behind any trusted proxy. When you save a config that enables the rate limiter and does not trust the proxy headers, a config advisory flags that combination.

Without trusted headers, the rate limiter keys on the address of the TCP connection. So it always has an IP: in a direct-to-internet deployment that address is the client, and behind a reverse proxy that is not trusted it is the reverse proxy. The admission chain's `source_ip_list` predicates and IAM `aws:SourceIp` conditions use the same client address.

## Codec semaphore

The codec semaphore limits the number of concurrent xdelta3 encode and decode subprocesses. Delta reconstruction (decode) uses little CPU time, but it is I/O-bound because it fetches the reference and the delta from storage. For this reason, the default is generous.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent xdelta3 processes | `num_cpus * 4` (min 16) | `DGP_CODEC_CONCURRENCY` |

Behavior differs by operation:

- **GET (decode)**: waits up to 60 seconds for a codec slot, then returns `503 SlowDown`.
- **PUT (encode)**: fails immediately with `503 SlowDown` when no slot is available, because a waiting upload would keep its body (in memory or in a spool file) for as long as it waits.

## HTTP concurrency limit

The proxy caps the total number of in-flight S3 API requests across the server. Requests beyond the limit wait in a queue until a slot opens or the request timeout fires, because the wait counts against the request timeout.

With more than one storage backend, the requests to one backend can hold at most a share of the slots (`DGP_BACKEND_SHARE_PERCENT`, 75 % by default). A request to a backend whose share is full gets `503 SlowDown` at once, and S3 clients retry it. A slow backend therefore cannot take every slot, and the requests to the other backends and to the admin UI keep their speed. The same share applies to the spool budget (`DGP_SPOOL_MAX_BYTES`): a request that needs spool space while its backend's share of the spool is full waits for that backend's own spool space, not for the whole budget.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent requests | 1024 | `DGP_MAX_CONCURRENT_REQUESTS` |
| Share of one backend | 75 % | `DGP_BACKEND_SHARE_PERCENT` |

## Spool share of one principal

A large delta GET reconstructs the object into a spool file and then streams that file. The response holds the file, and the spool budget that it uses, until the client has read the last byte. Without a limit, one user could start many large GETs, read none of them, and hold the whole spool budget, so that the large uploads of every other user wait and fail.

Five rules prevent this:

- **A cap for all reads together.** All reads together can hold at most `DGP_SPOOL_READ_MAX_PERCENT` of the spool budget (75 % by default). The writes keep the rest, 25 % of the budget with the default, however many downloads run. This is a reserve, not a promise for every object: an upload or multipart object that needs more spool space than that reserve can get a retryable `503 SlowDown` while reads hold their cap. `max_object_size: auto` allows an object of up to 37.5 % of the spool budget, so such an object is admitted but can wait. To make every admitted object find space, set `max_object_size` to the reserve, or lower `DGP_SPOOL_READ_MAX_PERCENT`. One read on an idle proxy can be larger than the cap: a single file that is larger than the cap can still be read when nobody holds any read space.
- **A share for each principal inside that cap.** The proxy counts the spool space that the reads of each principal hold. A principal is one access key, or all anonymous requests, or all requests when authentication is off. A read counts for as long as the response stream of its spool file lives, and for the principal of its own request. A spool file that only waits in the range-spool cache for a later request is not counted, because the proxy can remove it at any time. When one principal streams the same file in several requests, for example the ranges of a parallel download, the file counts once for that principal. When two principals stream the same cached file, the file counts once in the total of all principals, because it is on disk once and the read cap is about disk space. It counts in the own total of each of them, because each of them keeps it in use. So two principals can download one large file at the same time, as long as the file fits the read cap.
- **The same rule admits every read.** A read of a new file is admitted when one of two conditions holds. First, the space that the principal holds, plus the new file, is at most the share of the principal (`DGP_SPOOL_READ_SHARE_PERCENT`, 25 % of the read cap by default). Second, the space that all principals hold together, plus the new file, is at most the rest of the read cap (75 % of it by default). Because of the second condition, a principal that is alone can use 75 % of the read cap, which is 56 % of the spool budget with the defaults. Because of the first condition, every other principal can always reach its own share, as long as the read cap has room. Two principals together can fill the read cap, and no more. A read that is not admitted gets `503 SlowDown` at once, without a wait, and S3 clients retry it. The writes of other principals are not counted: they use the space that the read cap leaves.
- **The download that makes the file is charged first.** A GET of a large delta that no cached file serves reserves space for the delta and its reconstruction, waits for a codec slot, and decodes. The proxy asks the rule above before it reserves, downloads or decodes anything, with the space that the fill will reserve. The fill counts for the principal from that moment, and the charge of the response stream replaces it when the file is ready, so the file never counts twice. The fill counts everything that it reserves for the request: the output, the delta, and a copy of the reference that it downloads for itself. Each part stops counting as soon as its file is deleted, which is when the decode ends. A reference file that ends in the shared cache is evictable, like every cached file, so it counts only while it downloads. A principal that is over its reach gets `503 SlowDown` at once, and no spool file is made for it. A fill that fails or is cancelled gives its charge back. The same holds for a server-side copy: its read of a large source is a read of the requesting principal, for as long as the copy holds the source file. The proxy's own ingest paces a copy, not its client, so a client cannot slow a copy by stalling its connection.
- **Every door counts for its principal.** The S3 API counts reads for the principal of the request. The ZIP download and the bulk copy of the admin API count for the principal of the browser session: the access key of an IAM user (so the ZIP downloads and the S3 requests of one user share one ledger), `$open` when authentication is off, and a name of its own for the administrator who signed in with the bootstrap password. The background jobs (replication, lifecycle, maintenance) and the command-line verbs are not clients: their reads are not charged to a principal.

A response body that holds spool space also ends, and gives the space back, when its client takes too few bytes. In every window of `DGP_RESPONSE_STALL_SECS` seconds (60 by default) the client must accept a minimum number of bytes, unless the body has ended. The minimum is 64 KiB, or more for a large file: the smaller of the held spool space and the bytes that this response sends, times the window, divided by `DGP_RESPONSE_MAX_HOLD_SECS` (6 hours by default). A range request sends less than the file that it holds, so a range of 8 MiB of a 25 GiB file needs only 64 KiB per window, and a download of the whole 25 GiB file needs 71 MiB. So the proxy ends every response within about that time, whatever the client does. A client that reads one small chunk every 59 seconds is cut at the end of the first window. A client that takes more than the minimum is never cut, however long the download takes. The time that the proxy itself spends to wait for the storage backend or for the delta codec does not count against the client. A client that comes back after the body has ended gets a connection error, and retries with a new request.

The minimum rate that this gives with the defaults (a window of 60 seconds and 6 hours):

| Bytes that the response sends (up to the held spool space) | Minimum per window | Minimum rate |
|------------------|--------------------|--------------|
| 1 MiB | 64 KiB | 1.1 KiB/s |
| 100 MiB | 284 KiB | 4.7 KiB/s |
| 1 GiB | 2.8 MiB | 48.5 KiB/s |
| 10 GiB | 28.4 MiB | 485 KiB/s |
| 25 GiB | 71.1 MiB | 1.2 MiB/s |

The proxy counts the bytes that it hands to the network connection. The operating system buffers a few MiB of a response before the proxy sees that its client is slow, so a slow client is cut at the earliest after the buffered data is gone. The first window of a body therefore always passes.

The tables of limits that follow list what one principal and all principals can use with the defaults:

| What | One principal alone | Two principals together |
|------|---------------------|-------------------------|
| Reads (spool) | 56 % of the spool budget | the read cap, 75 % |
| Open multipart uploads | 750 of 1000 | 1000 (750 and 250) |
| Multipart part bytes | 75 % of `DGP_MAX_TOTAL_MULTIPART_BYTES` | all of it (75 % and 25 %) |
| Spool left for writes when reads hold their cap | 25 % of the spool budget | 25 % of the spool budget |

| Setting | Default | Env var |
|---------|---------|---------|
| Most of the spool budget that all reads hold together | 75 % | `DGP_SPOOL_READ_MAX_PERCENT` |
| Share of one principal of the read cap | 25 % | `DGP_SPOOL_READ_SHARE_PERCENT` |
| Window in which a response body that holds spool space must be read at its minimum rate | 60 s | `DGP_RESPONSE_STALL_SECS` |
| Time in which a held response body is released | 6 h | `DGP_RESPONSE_MAX_HOLD_SECS` |

## Connection limits

The request limit above counts requests in flight, so it does not count a connection that has not sent a complete request yet. The listener therefore has two limits of its own, for plain HTTP and for HTTPS alike.

A client has 30 seconds (`DGP_HEADER_READ_TIMEOUT_SECS`) to send the head of each request: the request line and the headers. The proxy closes a connection whose head does not arrive in that time, so a client that sends half a request and then waits cannot keep the connection open. The same limit closes an HTTP/1.1 keep-alive connection that sends no new request for 30 seconds. HTTP/2 has no such limit of its own, so the proxy enforces two limits itself. An HTTP/2 connection that has not received a successful response is closed after 30 seconds (the value of `DGP_HEADER_READ_TIMEOUT_SECS`) with no request in flight, so a client that opens connections and sends only the HTTP/2 preface, or only requests that the proxy refuses (a response with status 400 or more), cannot hold connection slots. An HTTP/2 connection that has already received at least one response with a status below 400 stays open for 120 seconds (`DGP_H2_IDLE_TIMEOUT_SECS`) with no request in flight, and is closed after that. The longer limit lets a reverse proxy reuse an idle connection: Traefik, for example, keeps idle upstream connections for 90 seconds, and a limit shorter than that would let a request race the close. In both cases the proxy closes the connection whatever the peer sends in the meantime (a ping does not count as a request). A connection with a request in flight is never closed by these limits. The proxy also pings every HTTP/2 connection every 30 seconds (the value of `DGP_HEADER_READ_TIMEOUT_SECS`), whether or not it has a request in flight, and closes a connection whose peer does not answer a ping within 120 seconds. The ping timeout is longer than the 30-second limits because, on a slow link, a ping can wait behind data that is still in the socket buffer, and a shorter timeout would cut a live transfer. The proxy closes an idle connection by closing the TCP connection. It does not send an HTTP/2 `GOAWAY` frame first, because the HTTP server library that the proxy uses gives no way to send one to a single connection (it sends `GOAWAY` to all connections only when the proxy shuts down). A client that sends a request at the very moment of the close sees that request fail with a connection error, and must send it again on a new connection. S3 clients and SDKs open a new connection when they need one and retry such an error, so this close is not an error for them. The request body is not under this limit; the request timeout covers it.

The proxy keeps at most a fixed number of connections open at the same time. By default, this number is four connections for each request slot (`DGP_MAX_CONCURRENT_REQUESTS`), and never fewer than 4096. `DGP_MAX_CONNECTIONS` sets the number directly. A connection that arrives while that many connections are open is closed at once, before the TLS handshake, and the metric `deltaglider_connections_refused_total` counts it. A slot frees when a connection closes.

One client address cannot take all of these slots. The proxy also limits the connections that come from one TCP peer address: by default a quarter of the connection limit, and at least one. `DGP_MAX_CONNECTIONS_PER_IP` sets the number directly. The proxy counts the address of the TCP connection, never a forwarded header. An IPv4 address counts by itself (an IPv4-mapped IPv6 address counts as its IPv4 address). An IPv6 address counts by its /64 network, because one IPv6 client owns a whole /64 and can use a new address for each connection. A connection above this limit is closed at once, in the same way as above, and the same metric counts it.

A connection from a network in `DGP_TRUSTED_PROXY_CIDRS` is not counted against this limit, because every client of a reverse proxy arrives from the address of that proxy. The limit of the whole listener (`DGP_MAX_CONNECTIONS`) still applies to it. Behind a reverse proxy, set `DGP_TRUSTED_PROXY_CIDRS` to the address or network of the proxy. If you do not, the proxy counts all clients as one address. The proxy then closes every connection above the per-address limit as soon as it arrives, and the reverse proxy answers its clients with `502 Bad Gateway`. In that case, either set `DGP_TRUSTED_PROXY_CIDRS` or raise `DGP_MAX_CONNECTIONS_PER_IP` to the number of connections that the reverse proxy opens.

| Setting | Default | Env var |
|---------|---------|---------|
| Request head timeout (and HTTP/1.1 keep-alive idle limit, idle limit of an HTTP/2 connection with no successful response yet, and HTTP/2 ping interval) | 30 s | `DGP_HEADER_READ_TIMEOUT_SECS` |
| Idle limit of an HTTP/2 connection that has received a successful response | 120 s | `DGP_H2_IDLE_TIMEOUT_SECS` |
| HTTP/2 ping timeout | 120 s | none (fixed) |
| Max open connections | 4 × `DGP_MAX_CONCURRENT_REQUESTS`, at least 4096 | `DGP_MAX_CONNECTIONS` |
| Max open connections for one peer address | A quarter of the max open connections, at least 1 | `DGP_MAX_CONNECTIONS_PER_IP` |

The proxy reads these variables only at startup. A change needs a restart.

## Request timeout

The proxy applies a deadline to each S3 API request. A request that exceeds the deadline gets HTTP `504 Gateway Timeout`. The time of a large delta reconstruction over a slow storage link counts toward this deadline.

| Setting | Default | Env var |
|---------|---------|---------|
| Request timeout | 300 s | `DGP_REQUEST_TIMEOUT_SECS` |

## Multipart upload limit

The proxy caps the number of concurrent in-progress multipart uploads. The proxy holds the parts of each upload until completion, in relay files in `DGP_SPOOL_DIR`, so the limit bounds the spool space that abandoned or excessive uploads can take. A CreateMultipartUpload request past the limit gets `503 SlowDown`.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent uploads | 1000 | `DGP_MAX_MULTIPART_UPLOADS` |
| Share of one principal | 25 % | `DGP_MULTIPART_PRINCIPAL_SHARE_PERCENT` |

The share of one principal follows the same rule as the spool share above, with the share `DGP_MULTIPART_PRINCIPAL_SHARE_PERCENT` (25 % by default) and without the clause for one large read. The rule applies to two budgets: the number of open uploads (`DGP_MAX_MULTIPART_UPLOADS`), and the spool space that the parts of all open uploads hold together (`DGP_MAX_TOTAL_MULTIPART_BYTES`). Without it, one user could open the maximum number of uploads, or fill the part bytes, and make every other user's multipart requests fail.

An open upload counts for the principal that created it. The bytes of a part count for the principal whose request sends the part, which can be another principal than the creator of the upload: UploadPart and UploadPartCopy requests pay for their own parts. This way a user cannot fill the share of another user by sending small parts into the other user's upload. Each part counts whole MiB, at least 1 MiB, and its bytes count until the part is replaced, or the upload completes, aborts or expires.

A request is admitted when one of two conditions holds: the uploads (or part bytes) of the principal plus the new one are at most its share, or all principals together stay within the rest of the cap (75 % by default). A principal that is alone can therefore use 75 % of each cap, and every other principal can still reach its own 25 %, as long as the whole cap has room. A request that neither condition admits gets `503 SlowDown`, and its message says that the limit is the caller's own. When the whole cap is full, a principal within its share gets the general `503 SlowDown` message, without the words about its own limit. Because one upload belongs to one principal, the largest multipart object that `max_object_size: auto` allows is what one principal can reach of the total: 75 % of `DGP_MAX_TOTAL_MULTIPART_BYTES` with the default share.

## Replay detection cache

The proxy caches the SigV4 signatures of mutating requests and rejects duplicates within the replay window. `DGP_CLOCK_SKEW_SECONDS` governs how far a request timestamp may drift from the server clock during SigV4 verification. The replay window defaults to that skew tolerance, because a signature outside the skew tolerance already fails verification: with the default, a captured mutating request is refused for its whole valid life.

| Setting | Default | Env var |
|---------|---------|---------|
| Replay window | twice the clock skew tolerance (1800 s) | `DGP_REPLAY_WINDOW_SECS` |
| Clock skew tolerance | 900 s | `DGP_CLOCK_SKEW_SECONDS` |
| Max cache entries | 500,000 | — |

The proxy rejects a duplicate of a mutating request (PUT/POST/DELETE) within the window with 400. The exception is a PUT or DELETE that arrives less than one second after the first copy. Such a request is an SDK retry inside the signing second, so the proxy serves it (see [Authentication and access](authentication.md)). An idempotent read (GET/HEAD) does not enter the cache at all. boto3 emits byte-identical signatures for the same request within one signing second, and a replayed read only re-reads the same bytes. Only a request that succeeds keeps its signature in the cache, so an SDK retry of a failed request is not a replay. Replay rejections do not count toward the auth-failure lockout. `DGP_REPLAY_WINDOW_SECS=0` disables replay rejection entirely. When the cache exceeds 500K entries, the proxy evicts the oldest signatures first. The cache is per instance: behind a load balancer, a replay that reaches another instance is not detected.

## S3 backend HEAD concurrency

During a LIST request that needs per-object metadata, the proxy sends HEAD requests to the upstream S3 backend. The proxy limits these requests so that they do not trigger the backend's own throttling.

| Setting | Default | Configurable |
|---------|---------|--------------|
| Max concurrent HEADs | 10 | No |

## Summary of all env vars

| Env var | Default | Description |
|---------|---------|-------------|
| `DGP_RATE_LIMIT_MAX_ATTEMPTS` | 100 | Auth failures before lockout |
| `DGP_RATE_LIMIT_WINDOW_SECS` | 300 | Rolling window for failure counting |
| `DGP_RATE_LIMIT_LOCKOUT_SECS` | 600 | Lockout duration after max failures |
| `DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS` | 10 | Failed sign-ins for one account, from any IP, before that account locks |
| `DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS` | 3600 | Rolling window for the per-account failure count |
| `DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS` | 3600 | Per-account lockout duration |
| `DGP_TRUST_PROXY_HEADERS` | false | Trust `X-Forwarded-For` / `X-Real-IP` for IP extraction (only behind a reverse proxy) |
| `DGP_TRUSTED_PROXY_CIDRS` | unset | Networks of the trusted reverse proxies; required when `DGP_TRUST_PROXY_HEADERS=true` |
| `DGP_CODEC_CONCURRENCY` | cpus*4 (min 16) | Max concurrent xdelta3 processes |
| `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Max in-flight S3 API requests |
| `DGP_BACKEND_SHARE_PERCENT` | 75 | With more than one backend, the most of the request slots and of the spool budget that the requests to one backend can hold |
| `DGP_REQUEST_TIMEOUT_SECS` | 300 | Per-request timeout |
| `DGP_SPOOL_READ_SHARE_PERCENT` | 25 | The share of one principal of the read cap, for open response streams and the downloads that fill them (1-100). A principal alone can use the rest of the read cap too (75 % of it by default); 100 turns the shares off |
| `DGP_SPOOL_READ_MAX_PERCENT` | 75 | The most of the spool budget that all reads together can hold (1-100); writes keep the rest. 100 turns the cap off |
| `DGP_RESPONSE_STALL_SECS` | 60 | The window in which a response body that holds spool space must be read at its minimum rate (64 KiB, more for a large file) |
| `DGP_RESPONSE_MAX_HOLD_SECS` | 21600 | A response body that holds spool space is released within this time whatever its client does; it sets the minimum rate of a large file |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Max concurrent multipart uploads |
| `DGP_MULTIPART_PRINCIPAL_SHARE_PERCENT` | 25 | The share of one principal of the upload cap and of the multipart part bytes (1-100). A principal alone can use the rest too (75 % by default); 100 turns the shares off |
| `DGP_CLOCK_SKEW_SECONDS` | 900 | SigV4 request-timestamp drift tolerance |
| `DGP_REPLAY_WINDOW_SECS` | 2 × `DGP_CLOCK_SKEW_SECONDS` (1800) | SigV4 replay detection window for mutating requests (0 disables) |

## Related

- [Authentication and access](authentication.md): SigV4 verification and replay-detection semantics
- [Configuration](configuration.md): the full env-var registry
