# baton-relay

A Cloudflare Worker that forwards Baton's end-to-end encrypted
notifications to APNs. It holds the APNs key, never sees plaintext, and
stores nothing except short-lived, hashed rate-limit counters (one Durable
Object per key). The protocol is described in [`docs/relay.md`](../docs/relay.md).

```
POST /v1/register  {device_token, env}          → background push carrying a send credential
POST /v1/send      {device_token, env, credential, ciphertext, collapse_id?, priority?, ttl?}
GET  /v1/health
```

## Develop

```sh
npm install
npm test            # vitest inside workerd (Miniflare); APNs is mocked
npm run typecheck
npx wrangler dev    # local; put test secrets in .dev.vars (gitignored)
```

## Deploy

You need a Cloudflare account (the free plan works; Durable Objects use
the SQLite backend) and an Apple Developer account.

1. **APNs key.** In Apple Developer → Certificates, IDs & Profiles → Keys,
   create a key with *Apple Push Notifications service (APNs)* enabled and
   download the `.p8` file (it can only be downloaded once). Note the
   **Key ID** and your **Team ID**. One key works for both sandbox and
   production.
2. **Log in.** Run `npx wrangler login`. `wrangler.jsonc` has no account id:
   wrangler asks on the first deploy, or you can set `CLOUDFLARE_ACCOUNT_ID`.
3. **Secrets.** Nothing is committed. Set each one:

   ```sh
   npx wrangler secret put APNS_KEY < AuthKey_XXXXXXXXXX.p8   # the .p8 contents (PEM)
   npx wrangler secret put APNS_KEY_ID                        # e.g. ABC123DEFG
   npx wrangler secret put APNS_TEAM_ID                       # e.g. 1A2B3C4D5E
   npx wrangler secret put APNS_TOPIC                         # the iOS app's bundle id
   openssl rand -base64 32 | npx wrangler secret put REGISTRATION_SECRET
   ```

   Rotating `REGISTRATION_SECRET` later revokes every device's send
   credential. Phones then re-register.
4. **Deploy** with `npx wrangler deploy`. The relay is served at
   `https://baton-relay.<subdomain>.workers.dev`, or you can add a custom
   domain route.
5. **Point the daemon at it** with `BATON_RELAY_URL=https://…`, or
   `baton serve --relay https://…`, or `relay=https://…` in the daemon's
   `--config` file. The iOS app needs the same URL for `/v1/register`.

`GET /v1/health` returns `{"ok":true,"protocol":1,"configured":true}` once
all the secrets are set.

### First deploy check

The tests mock APNs, so the first real send needs checking once. Use a
development build of the app (sandbox): pair it, then trigger a
notification (for example let a Claude session stop). Then look at
`npx wrangler tail`. A `200` from APNs means the Worker reached it over
HTTP/2. Any other outcome is logged by the relay as `apns_error` together
with Apple's `reason`.

## Self-hosting notes

APNs only delivers to apps signed by the key's team with a matching
bundle id (`APNS_TOPIC`). A self-hosted relay therefore pairs with an app
build you sign yourself. The relay only ever sends to `APNS_TOPIC`, even
if the key is valid for other apps of the team.

## Limits (src/ratelimit.ts)

| What | Limit |
|---|---|
| sends per (env, token) | 20 per minute, 300 per hour |
| registrations per (env, token) | 5 per hour |
| registrations per client IP | 30 per hour |
| request body | 8 KiB |
| ciphertext | 48 to 2048 bytes |
