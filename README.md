# Replex

Remix your plex hubs

![plot](./examplewithhero.png)

### Looking for maintainers

I moved away from Plex and therefore this project is in need of an maintainer.

## Features

- Merge hubs (recommended rows) from different libraries into a [single hub](./interleave.png) (interleave). Aka have movies and shows in a single row.
- Choose between styles, [shelf](./shelf.png) (default) or [hero](./hero.png).
- Remove watched items from hubs.
- Auto load artwork for hero styles.
- Filter hubs by its restrictions (per user hub)
- Disable user state: remove watched badges from hub items.
- Disable leaf count: remove episode count from artwork.
- Force maximum quality.
- Auto select version based on resolution of the client.
- Fallback to different version if selected version is video transcoding.
- Works on every client not only plex web!
- Plays nice with PMM (and without).

## How does it work

Replex is an proxy that transforms the communication between the plex media server and plex clients. 
This allows replex to change some dials that otherwise wouldnt be possible.

## Installation

Docker compose example including plex:

```yml
version: "3"
services:
  plex:
    image: lscr.io/linuxserver/plex:latest
    container_name: plex
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=Etc/UTC
      - VERSION=docker
      # claim from https://plex.tv/claim 
      - PLEX_CLAIM=
    ports:
      - 32400:32400
     volumes:
       - /path/to/library:/config
       - /path/to/tvseries:/tv
       - /path/to/movies:/movies
    restart: unless-stopped
  replex:
    image: ghcr.io/lostb1t/replex:latest
    container_name: replex
    environment:
      REPLEX_HOST: http://plex:32400
      REPLEX_TOKEN: ***** # server admin plex token: https://support.plex.tv/articles/204059436-finding-an-authentication-token-x-plex-token/
    ports:
      - 3001:80
    restart: unless-stopped
    depends_on:
      - plex
```

Add your proxy url to plex "Custom server access URLs" (ex http://0.0.0.0:3001)
Then access your proxy url http://0.0.0.0:3001

Note: DO NOT run the plex container in host mode. It will cause plex to connect to the local ip instead of the custom url for
local clients and bypass replex.

Note: Plex clients are a bit broken with custom urls and unsecured connections. Most wont work if the custom server url is not secure.
So for testing the direct web client is fine but after that you want to setup SSL. See the "Remote access" section for more info.

## Settings

Settings are set via [environment variables](https://kinsta.com/knowledgebase/what-is-an-environment-variable/) 

| Setting        	          | Default 	| Description                                                            	  |
|---------------------------|----------|---------------------------------------------------------------------------|
| REPLEX_HOST               |        	 | Url of your plex instance. ex: http://0.0.0.0:32400                                             	  |
| REPLEX_TOKEN              |        	 | server admin plex token, needed for hero images. To find your token see: https://support.plex.tv/articles/204059436-finding-an-authentication                                      	  |
| REPLEX_INTERLEAVE         | true      | Interleave home hubs. Collection hubs with the same name from different libraries are interleaved (combined) into one.                                           	  |
| REPLEX_EXCLUDE_WATCHED    | true    | If set to true, hide watched items for hubs.                                    |
| REPLEX_HUB_RESTRICTIONS   | true      | Apply collections restrictions to their hub's. Plex does not apply restrictions to hubs, so you cannot have different collection hubs for users. this fixes that.                                       	  |
| REPLEX_DISABLE_CONTINUE_WATCHING | false    | Disable/remove the continue watching row |
| REPLEX_DISABLE_USER_STATE | true    | Remove watched badges from hub items. * does not work on all clients |
| REPLEX_DISABLE_LEAF_COUNT| false    | Remove episode count label from show artwork.                              |
| REPLEX_HERO_ROWS          |        	 | Comma seperated list of hubidentifiers to make builtin hubs hero style. For custom collections see [Hhb style](#-hub-style).  Options are: <br />home.movies.recent<br />movies.recent <br />movie.recentlyadded<br />movie.topunwatched<br />movie.recentlyviewed<br />hub.movie.recentlyreleased<br />movie.recentlyreleased<br />home.television.recent<br />tv.recentlyadded<br />tv.toprated<br />tv.inprogress<br />tv.recentlyaired    |
| REPLEX_FORCE_MAXIMUM_QUALITY    | false    | This will force clients to use the maximum quality. Meaning that if a client requests anything other then the maximum quality this will be ignored and the maximum quality (direct play/stream when server allows for original) is used instead. This doesn't prevent transcoding. It only sets the bitrate to original quality. So if a client needs a different codec, container or audio it should still transcode. 
| REPLEX_FORCE_DIRECT_PLAY_FOR    | false    | Force direct play for the given resolutions. Options are "4k", "1080" and "720".  This wil result in an error message if the client does not support directplay. Not recommended      
| REPLEX_VIDEO_TRANSCODE_FALLBACK_FOR    |     | If the selected media triggers a video transcode. Fallback to another version of the media. Only triggers on video transcoding. Remuxing is still allowed. <br />Options are "4k" and "1080". <br /> <br /> Example if  REPLEX_VIDEO_TRANSCODE_FALLBACK_FOR is set to "4k" then 4k transcodes will fallback to another version if avaiable |
| REPLEX_AUTO_SELECT_VERSION    | false    | If you have multiple versions of a media item then this setting will choose the one thats closest to the client resolution. So a 1080p TV will get the 1080P version while 4k gets the 4k version. A user can still override this by selecting a different version from the client.   |
| REPLEX_DISABLE_RELATED  | false | See: https://github.com/lostb1t/replex/issues/26.        |
| REPLEX_REDIRECT_STREAMS  | false    | For **unlimited** accounts, 302-redirect stream bytes directly to the Plex origin (best performance). Restricted accounts are **always proxied through Replex** regardless of this setting, so their resolution limit stays enforceable. Set `false` to proxy every stream. |
| REPLEX_REDIRECT_STREAMS_HOST  | REPLEX_HOST    | Alternative streams endpoint                                         |
| REPLEX_CACHE_TTL          | 1800    	 | Time to live for general caches in seconds. Set to 0 to disable (higly recommended to keep enabled besides testing purposes).  |
| REPLEX_WARM_INTERVAL      | 300    	 | Seconds between background warmer cycles that pre-fetch hot hub payloads with the admin token so clients never pay the slow cold fetch. 0 disables warming.  |
| REPLEX_WARM_TOKENS      |    	 | Comma separated list of extra Plex tokens to pre-warm. The warmer fetches each token's hubs/library into its own user-scoped cache scope, so accounts other than the configured admin also get cold-start-free loads. When empty, only `REPLEX_TOKEN` (admin) is warmed. |
| REPLEX_HUB_STALE_TTL      | 300    	 | Hub payloads older than this (seconds) are served instantly while being refreshed in the background, so clients never wait on a slow upstream fetch. Playback changes seen through the proxy (scrobbles, playback stopping) mark all hubs stale immediately, keeping Continue Watching fresh within seconds. Set to 0 to disable the staleness layer.  |

## Hub caching and freshness

Hubs are served from a shared cache that all users read from. Responses are
always instant: when a cached payload is past `REPLEX_HUB_STALE_TTL` it is
still served while a background refresh updates it for the next request.
When a client reports playback changes through the proxy (`/:/scrobble`,
`/:/timeline` with state=stopped), all hub payloads are marked stale so the
next request refreshes them — this keeps Continue Watching and On Deck
accurate within seconds without ever blocking a client on the upstream
Plex server, which can take many seconds (or worse) to regenerate promoted
hubs.

Optionally, if the server owner has Plex Pass, adding `http://<replex>/replex/webhooks`
to the Plex webhooks settings marks the cache stale on Plex events too
(useful for changes made outside the proxy; requires a publicly reachable URL).

## Web UI asset caching

Plex serves its static web files (`/web/*`) with `Cache-Control: no-cache` and
no validators, forcing browsers to re-download all of them on every reload.
Replex caches these immutable content-hashed files in memory and serves them
with `Cache-Control: public, max-age=31536000, immutable`, so repeat loads of
the web app skip the upstream round trip entirely (`index.html` and
translations stay short-lived so app updates still propagate).


## Per-user resolution restrictions

Restrict individual Plex accounts to a maximum media resolution while keeping
everything in one library. 1080p and 4K versions stay merged under the same
item; restricted accounts simply never see or reach the versions above their
limit.

> **Intent: enforced by Replex, provided the Plex origin is isolated.** Restricted
> accounts' streams are proxied through Replex and direct/unknown part requests
> are blocked, so the limit holds for any client that talks to Plex *through*
> Replex. It is still **not** a hard security boundary against a client that can
> reach the Plex server directly: anyone with direct network access and an
> accepted token can construct requests that bypass these restrictions. For hard
> enforcement, firewall the Plex origin so only Replex can talk to it — Replex
> alone cannot substitute for that network control.

```text
REPLEX_RESOLUTION_POLICY_ENABLED=true

REPLEX_USER_RESOLUTION_POLICIES=[{"username": "jodie", "max_resolution": "1080"},
                                 {"username": "luke", "max_resolution": "4k"}]

REPLEX_RESOLUTION_DEFAULT=unlimited
REPLEX_RESOLUTION_POLICY_FAIL_CLOSED=true
REPLEX_STRICT_STREAM_GUARD=false
```

| Setting | Default | Description |
|---|---|---|
| REPLEX_RESOLUTION_POLICY_ENABLED | false | Master switch. When false, behaviour is identical to stock Replex and the metadata routes are not even registered. |
| REPLEX_USER_RESOLUTION_POLICIES | | JSON array of per-account rules. Each entry needs `username` and/or `uuid` plus `max_resolution` (`480`, `720`, `1080`, `4k`, `unlimited`). Optional `max_bitrate` (kbps) caps playback bitrate for that account — it applies even when the resolution is `unlimited`, and requests above the cap are lowered while lower requests are left alone. Optional `visible_collections` lists collection titles this account can see despite the global hidden default — everyone else has them hidden. UUID is the stable identifier (visible in server logs at identity resolution time); username matching is case sensitive. |
| REPLEX_RESOLUTION_DEFAULT | unlimited | Limit applied to accounts without an explicit rule. |
| REPLEX_RESOLUTION_POLICY_FAIL_CLOSED | true | If the account identity cannot be verified (plex.tv unreachable, invalid token) playback requests fail with 503 instead of being allowed unrestricted. Cached identities mean brief plex.tv outages are invisible. |
| REPLEX_STRICT_STREAM_GUARD | false | Reject direct `/library/parts` requests for parts Replex has never seen in metadata or playback (hand-crafted deep links). Disabled keeps legacy behaviour for unknown parts. |
| REPLEX_HIDDEN_COLLECTIONS | | Comma separated list of collection titles hidden from **all** accounts. Accounts with a matching `visible_collections` entry in their policy see them normally. Exact title match (case sensitive, emoji included). |
| REPLEX_IDENTITY_CACHE_TTL | 3600 | How long verified account identities are cached, seconds. |
| REPLEX_IDENTITY_API_BASE | https://plex.tv | Identity API override, for testing only. |

How it works:

* The request's own Plex token is verified against plex.tv to identify the
  account — client-supplied usernames are ignored.
* Metadata responses have prohibited versions removed before clients see
  them; items that only exist above the limit disappear entirely.
* Playback requests with a version above the limit are rewritten to the best
  permitted version; transcode fallback can never cross the limit.
* Direct media part URLs belonging to prohibited versions return 403; for restricted accounts, **unknown** parts (Replex has never seen in metadata/playback) are blocked too. Restricted accounts' part and transcode-session streams are proxied through Replex rather than 302-redirected, so the client never receives the Plex origin URL.
* Accounts without a rule (or `unlimited`) behave exactly as stock Replex; their streams may be 302-redirected straight to the origin for performance when `REPLEX_REDIRECT_STREAMS` is enabled.

Requirements and limitations:

* All client traffic must flow through Replex. Clients that connect directly
  to the Plex server bypass every restriction: disable GDM, block direct
  access where possible, and set Replex as the Custom server access URL
  (see [Remote access](#remote-access-force-clients-to-use-the-proxy)).
  See the intent note above: these limits are convenience features — for
  hard enforcement, restricted clients must have no route to the Plex
  origin at all.
* Only invited shared accounts are supported. Home/managed users authenticate
  differently and are not covered.
* This restricts which source files may be accessed, it does not transcode a
  4K file down to 1080p for restricted users.
* Remote playback quality limits configured inside Plex itself still apply on
  top of these policies.
* Per-account `max_bitrate` is incompatible with the global
  `REPLEX_FORCE_MAXIMUM_QUALITY` — that setting strips bitrate parameters for
  everyone and will override individual caps. Do not enable both.

## Interleaved rows

Collections hubs with the same name from different libraries will be merged into one on the home screen.
So an collection hub named "Trending" in the Movie library will be merged with an collection named "Trending" from a shows library on home.

Note, this does not work on builtin hubs. As i personally dont see then need of mixing those. 
You can recreate the builtin rows with smart collections if you wish to have that functionality, or with PMM ofcourse.

## Hub style

For custom collections you can change the hub style to hero by setting the label "REPLEXHERO" on an collection.

For built in rows you can use the hubidentifier in the `REPLEX_HERO_ROWS`. See the setting for available know options.

Note: hero style elements uses coverart from plex. Banner or background is not used.
Note: Hero elements are not supported for continue watching by plex. You can replicate this functionality by creating a smart collection which filters on in progress and settinf REPLEX_DISABLE_CONTINUE_WATCHING

## Exclude watched items

If you want to hide watched items from your hubs, you can set `REPLEX_EXCLUDE_WATCHED` to true. Alternatively, you can add the label "REPLEX_EXCLUDE_WATCHED" to a collection to exclude watched items from that collection only.

## Remote access (force clients to use the proxy)

Because this app sits before Plex the builtin remote access (and auto SSL) will not work and needs to be disabled.

For testing purposes you can access through the browser at http://[replexip]:[replexport] (ex: http://localhost:3001)
But if you want other clients to connect to replex you need to setup a reverse proxy with a domain and preferable ssl.

A few easy to setup reverse proxys are: https://caddyserver.com or https://nginxproxymanager.com

Once you have your domain hooked up to replex add your replex url to 'Custom server access URLs' field under network.
and lastly disable remote access under remote access. 

Clear you clients caches to force plex reloading the custom server url

Note: SSL is highly suggested, some clients default to not allowing insecure connections. And some clients dont even support insecure connections (app.plex.tv)


## Reverse proxy

There should be no need for this but if you have a reverse proxy running and dont want to proxy streaming through plex then you can route the following paths and it subpaths directly to plex.

- /video/:/transcode/universal/session
- /library/parts

## Redirect streams

If you have for example an appbox it might not be ideal to stream media through replex. As that will take a lot of network resources.
You can redirect streams by enabling `REPLEX_REDIRECT_STREAMS` and optionally set `REPLEX_REDIRECT_STREAMS_HOST` if it needs to be different from REPLEX_HOST

Note: Plex doesnt handle redirects wel, and will not remeber it. So every chuck of a stream will first hit replex and then gets redirected to actuall download that chuck from the redirect url. So a bit wastefull

## Known limitations

- hero hubs on Android devices dont load more content. so hero hubs have a maximum of 100 items on Android.
- On android mobile hero elements in libraries are slightly cutoff. This is plex limitation.
- when exclude_watched is true a maximum item limit per library is opposed of 250 items. So if you have a mixed row of 2 libraries the max results of that row will be 500 items.
- disable_user_state: For movies this works in the webapp. Shows work accross clients

## Help it doesnt work!

### Replex works on on app.plex.tv but not on my clients

- disable GDM in plex and make sure plex is not directly acccesible. you can use this url to check what servers plex communicates to your clients: https://clients.plex.tv/api/v2/resources?includeIPv6=1&includeRelay=1&X-Plex-Language=en-NL&X-Plex-Token=YOURTOKEN&X-Plex-Client-Identifier=1234
- Try to clear the cache on the client. Old plex domains might linger.
