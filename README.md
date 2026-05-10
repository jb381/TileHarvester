![TileHarvester playful title banner](assets/tileharvester-title-variant-f-playful-harvester.png)

# TileHarvester 🗺️✨

Auto-drop Squadrats stats into your Strava descriptions. No scraping, no spam — just vibes.

## What it does

Every new activity gets a little line in the description:

```
🗺️ TileHarvester: 1,234 Squadrats · +13 new · +653/mo · +10/wk
```

Your friends will know you're grinding tiles 🚴‍♂️🏃‍♀️

## Quick start (UV)

```bash
# 1. grab it
git clone <repo> && cd tileharvester
uv sync

# 2. grab Strava creds → https://www.strava.com/settings/api
export TH_STRAVA_CLIENT_ID="your-id"
export TH_STRAVA_CLIENT_SECRET="your-secret"

# 3. log in
uv run tileharvester auth

# 4. build your history
uv run tileharvester backfill

# 5. test it
uv run tileharvester sync --once
```

Then either leave `uv run tileharvester sync` running, or use Docker/systemd for fire-and-forget.

## Docker (set it and forget it) 🐳

```bash
# 1. grab it
git clone <repo> && cd tileharvester

# 2. create your env
cp .env.example .env
# edit .env and add your Strava creds

# 3. build
docker compose build

# 4. one-time setup
docker compose run --rm tileharvester auth
docker compose run --rm tileharvester backfill

# 5. start background sync (checks every 5 minutes)
docker compose --profile cron up -d tileharvester-cron

# 6. watch the logs
docker compose --profile cron logs -f tileharvester-cron
```

Data survives in a Docker volume (`tileharvester-data`).

### Updating later

```bash
git pull
docker compose build
docker compose --profile cron up -d tileharvester-cron
```

## Common commands

| Command | What it does |
|---------|--------------|
| `tileharvester auth` | Strava login 🔓 |
| `tileharvester backfill` | One-time history build 📚 |
| `tileharvester sync --once` | Single sync 🔄 |
| `tileharvester sync` | Keep watching 👀 |
| `tileharvester status` | What's up 📊 |

Run `tileharvester --help` for the full menu.

## Why your counts might be off 🎯

By default, TileHarvester uses **summary polylines** (simplified GPS lines) from Strava to compute tiles. Polylines cut corners and skip points, which makes routes cross extra tile boundaries and **overcount tiles**. The longer the activity, the bigger the drift.

For accurate counts that match Squadrats, you should **refine** your data. This re-fetches the full GPS stream for each activity and recomputes tiles from the actual recorded points.

```bash
# Refine historical activities using full GPS streams
# This also rebuilds global totals automatically — no need to run recompute after
uv run tileharvester refine
```

Or with Docker:

```bash
docker compose run --rm tileharvester refine
```

> ⚠️ **Don't run `recompute` after `refine`** — `recompute` rebuilds from summary polylines and will undo your refinement.

Check your refinement status with `tileharvester status` — look for the "Stream-refined" vs "Needs stream refinement" counts.

### Manual offset

If counts still drift after refinement, you can nudge the lifetime total:

```bash
# Bump your lifetime Squadrat count by +5 on the next sync
uv run tileharvester sync --once --offset +5
```

For the cron job, set it in your `.env`:

```bash
TH_SQUADRAT_OFFSET=+5
```

That only adjusts the lifetime total shown in descriptions. Weekly and monthly counts are derived from your local DB and can't be tweaked individually.

## License 📄

MIT — use it, fork it, whatever.
