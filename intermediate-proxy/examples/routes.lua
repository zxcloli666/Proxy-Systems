-- intermediate-proxy routing config. Hot-reloaded on mtime change; a parse
-- error keeps the previous config running (never half-applied).
--
-- return { defaults = {...}, routes = { {...}, ... } }
--
-- Route fields:
--   name          string
--   match         { host | host_regex, path | path_regex, methods, predicate }
--   pool          { tags = {..all required..}, any_tags = {..at least one..} }
--   selector      "best_latency" | "round_robin" | "random"
--                 | "sticky_by_header" (+ sticky_header) | "hedge"
--   hedge         { max_parallel = N, delay_ms = N }   (selector="hedge")
--   max_attempts, timeout_ms, slow_ms, fail_streak,
--   attempt_backoff_ms, capture_body_bytes, prefer_non_reserve,
--   default_ban_ms
--   on_request(ctx)            -> { add_headers, drop_headers,
--                                   override_pool = {tags=...}, force_proxy }
--   on_response(ctx, res)      -> verdict (see below)
--   on_exhausted(ctx)          -> { verdict = "try_fatal" | "wait_probe"
--                                   | "retry_all" | "return_error" }
--   probe         { url, interval_ms, ok_statuses, classify(status,h,body) }
--
-- on_response verdicts:
--   { verdict = "success" }
--   { verdict = "return_as_is" }                       -- target's fault, proxy fine
--   { verdict = "retry_other_proxy", ban_for_ms = N }  -- ban proxy on THIS route
--   { verdict = "retry_same_proxy", delay_ms = N }
--   { verdict = "hard_fail", reason = "..." }
--   { verdict = "give_up", status = N, body = "..." }
--
-- ctx.headers / res.headers keys are lower-cased.

local function is_cloudflare(res)
  local s = (res.headers["server"] or ""):lower()
  return s:find("cloudflare") ~= nil or res.headers["cf-ray"] ~= nil
end

-- Genius: 429 behind Cloudflare = proxy IP banned; 429 with a rate-limit
-- header = the token is throttled and the proxy is fine.
local function genius_classify(ctx, res)
  if res.status == 429 then
    if is_cloudflare(res) then
      return { verdict = "retry_other_proxy", ban_for_ms = 10 * 60 * 1000 }
    end
    return { verdict = "return_as_is" }
  end
  if res.status >= 500 then
    return { verdict = "retry_other_proxy", ban_for_ms = 2 * 60 * 1000 }
  end
  return { verdict = "success" }
end

return {
  defaults = {
    max_attempts = 5,
    timeout_ms   = 10000,
    slow_ms      = 3000,
    fail_streak  = 2,
    selector     = "best_latency",
    default_ban_ms = 5 * 60 * 1000,
    hedge        = { max_parallel = 3, delay_ms = 600 },
  },

  routes = {
    -- SoundCloud api-v2: usually a global IP/anon ban (5xx / 401 / 403),
    -- rotate proxies and ban the bad one for a while.
    {
      name = "sc-apiv2",
      match = { host = "api-v2.soundcloud.com" },
      pool = { tags = { "anon" } },
      selector = "round_robin",
      max_attempts = 4,
      on_response = function(ctx, res)
        if res.status == 401 or res.status == 403 then
          return { verdict = "retry_other_proxy", ban_for_ms = 30 * 60 * 1000 }
        end
        if res.status >= 500 or res.status == 429 then
          return { verdict = "retry_other_proxy", ban_for_ms = 10 * 60 * 1000 }
        end
        return { verdict = "success" }
      end,
      probe = {
        url = "https://api-v2.soundcloud.com/resolve?url=https%3A%2F%2Fsoundcloud.com",
        interval_ms = 10 * 60 * 1000,
        ok_statuses = { 200, 401 }, -- 401 = proxy fine, token bad
      },
    },

    -- SoundCloud api-v1: 429 is usually token-level (proxy is fine);
    -- only 5xx means the proxy itself is blocked.
    {
      name = "sc-apiv1",
      match = { host = "api.soundcloud.com" },
      on_response = function(ctx, res)
        if res.status >= 500 then
          return { verdict = "retry_other_proxy", ban_for_ms = 5 * 60 * 1000 }
        end
        if res.status == 429 then
          return { verdict = "return_as_is" }
        end
        return { verdict = "success" }
      end,
      probe = {
        url = "https://api.soundcloud.com/tracks/308946217",
        interval_ms = 5 * 60 * 1000,
        ok_statuses = { 200, 401, 403 },
      },
    },

    -- SoundCloud media (HLS / progressive): hedge for speed, recover streams.
    {
      name = "sc-media",
      match = { host_regex = "%.sndcdn%.com$" },
      selector = "hedge",
      hedge = { max_parallel = 3, delay_ms = 400 },
      max_attempts = 6,
      on_response = function(ctx, res)
        if res.status >= 500 or res.status == 403 then
          return { verdict = "retry_other_proxy", ban_for_ms = 60 * 1000 }
        end
        return { verdict = "success" }
      end,
    },

    -- Genius token API: only requests carrying Authorization.
    {
      name = "genius-api",
      match = {
        host = "api.genius.com",
        predicate = function(req) return req.headers["authorization"] ~= nil end,
      },
      selector = "round_robin",
      max_attempts = 3,
      on_response = genius_classify,
      probe = { url = "https://api.genius.com/songs/1", ok_statuses = { 200, 401 } },
    },

    -- Genius web scrape: HTML, do NOT hedge (would 4x the ban rate).
    {
      name = "genius-scrape",
      match = { host = "genius.com" },
      pool = { tags = { "anon" } },
      selector = "best_latency",
      on_response = function(ctx, res)
        if res.status == 429 or res.status == 403 then
          return { verdict = "retry_other_proxy", ban_for_ms = 10 * 60 * 1000 }
        end
        if res.status >= 500 then
          return { verdict = "retry_other_proxy", ban_for_ms = 2 * 60 * 1000 }
        end
        return { verdict = "success" }
      end,
      probe = { url = "https://genius.com/Sample-lyrics", ok_statuses = { 200 } },
    },

    -- Generic lyrics sites: slow, lenient timeout, no hedge.
    {
      name = "lyrics-generic",
      match = { host_regex = "^(www%.)?(azlyrics|lyrics|musixmatch)%.com$" },
      timeout_ms = 20000,
      on_response = function(ctx, res)
        if res.status >= 500 or res.status == 429 then
          return { verdict = "retry_other_proxy", ban_for_ms = 5 * 60 * 1000 }
        end
        return { verdict = "success" }
      end,
    },

    -- Catch-all. Without this, an unmatched host returns 502 immediately.
    {
      name = "default",
      match = { host = "*" },
      on_response = function(ctx, res)
        if res.status >= 500 then
          return { verdict = "retry_other_proxy", ban_for_ms = 60 * 1000 }
        end
        if res.status == 429 then
          return { verdict = "retry_other_proxy", ban_for_ms = 5 * 60 * 1000 }
        end
        return { verdict = "success" }
      end,
      on_exhausted = function(ctx)
        return { verdict = "try_fatal" }
      end,
    },
  },
}
