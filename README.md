![TileHarvester playful title banner](assets/tileharvester-title-variant-f-playful-harvester.png)

# TileHarvester 🗺️✨

Auto-drop Squadrats stats into your Strava descriptions. No scraping, no spam — just vibes.

## What it does

Every new activity gets a little line in the description:

```
TileHarvester: +13 new Squadrats · +653 this month · +10 this week
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
docker compose run --rm tileharvester auth
docker compose run --rm tileharvester backfill
docker compose --profile cron up -d tileharvester-cron
```

Checks every 5 minutes. Data survives in a Docker volume.

## Common commands

| Command | What it does |
|---------|--------------|
| `tileharvester auth` | Strava login 🔓 |
| `tileharvester backfill` | One-time history build 📚 |
| `tileharvester sync --once` | Single sync 🔄 |
| `tileharvester sync` | Keep watching 👀 |
| `tileharvester status` | What's up 📊 |

Run `tileharvester --help` for the full menu.

## Numbers looking off? 🎯

Tile counts are computed from Strava GPS streams — they should be close, but don't always match Squadrats' official numbers pixel-for-pixel. If your lifetime total drifts, you can nudge it:

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
