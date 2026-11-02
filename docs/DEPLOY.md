# Deploying the commons

The commons (`speedrun-commons`, `COMMONS.md` §19–§20) runs as one Railway service built from
the repository's `Dockerfile`, with `railway.json` setting the health check and restart policy.
Its state is two files on a volume mounted at `/data`: `commons.sqlite` (the log, judgments,
people and links) and `commons.key` (the key that signs judgments, tree heads and links).

One replica only: SQLite has one writer, and the commons serializes publications in-process.

## Variables

| Variable | Value |
|---|---|
| `SPEEDRUN_COMMONS_HOME` | `/data` (the image sets it; the volume must mount there) |
| `PORT` | set by Railway; the commons listens on `0.0.0.0:$PORT` |
| `SPEEDRUN_COMMONS_PUBLIC_URL` | the service's `https://…up.railway.app` address, for the run page's `curl` commands |
| `SPEEDRUN_GITHUB_CLIENT_ID` | the client id of the GitHub OAuth app, with device flow enabled; without it linking answers 503 |
| `RUST_LOG` | optional, e.g. `info` |

The GitHub OAuth app is created by its owner at GitHub → Settings → Developer settings → OAuth
Apps, with "Enable Device Flow" ticked. Device flow needs no client secret, and the commons
never stores a GitHub token.

## First deploy

```bash
railway init --name agentspeedrun-commons
railway add --service commons --json
railway volume add --service commons --mount-path /data --json
railway variable set --service commons SPEEDRUN_GITHUB_CLIENT_ID=<client id>
railway domain --service commons
railway variable set --service commons SPEEDRUN_COMMONS_PUBLIC_URL=https://<generated domain>
railway up --service commons --detach -m "Deploy the commons"
railway deployment list --service commons --json
```

The deploy is done only when the newest deployment's status is `SUCCESS`. The first start
creates `commons.key`; copy it somewhere safe at once (it is the commons' identity, and a lost
key means every later judgment is signed by a stranger):

```bash
railway ssh --service commons -- cat /data/commons.key
```

## Every later deploy

1. Back up the database, on the volume and then off it:

   ```bash
   railway ssh --service commons -- 'mkdir -p /data/backups && speedrun-commons backup /data/backups/commons-$(date -u +%Y%m%dT%H%M%SZ).sqlite'
   railway volume files --service commons
   ```

   `backup` prints `backed up <n> leaves to <file>` and refuses to overwrite a file.
2. Deploy: `railway up --service commons --detach -m "<summary>"`, then poll
   `railway deployment list --service commons --json` until the newest status is `SUCCESS`.
3. Smoke test the live site (below). A defect found there gets a regression test before the
   redeploy.

## Smoke test

```bash
curl -fsS https://<domain>/v1/health
curl -fsS https://<domain>/v1/log/head
```

Then, in a browser at 320 px and at desktop width: `/` lists publications, a run page shows its
clocks, judgments and record, and `/people/<login>` shows a linked person. From a machine with a
finished run, `speedrun publish --tier 1 --run <id> --to https://<domain>` prints `published`, a
leaf line, and one line per milestone; `speedrun link --to https://<domain>` completes at GitHub.

## Restore

Stop the service, replace `/data/commons.sqlite` with a backup (and `/data/commons.key` with the
saved key if it was lost), and start it. `/v1/health` must report the backup's tree size, and
`/v1/log/head` the root it had.
