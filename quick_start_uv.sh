#!/bin/bash
# 1. grab it
if [ "TileHarvester" != $(basename "$PWD") ]; then
    git clone https://github.com/jb381/TileHarvester.git && cd TileHarvester
    cd TileHarvester
fi

python3 -m venv venv
source venv/bin/activate
pip install uv
uv sync

# 2. grab Strava creds → https://www.strava.com/settings/api
echo "Welcome to TileHarvester! Let's get you set up."
echo "First, we need your Strava API credentials. You can get these from https://www.strava.com/settings/api"
echo "Don't worry, we won't share these with anyone. They're just for you to use TileHarvester! Trust us bro."
echo "Gimme your Strava Client ID: "
read -r client_id
echo "Gimme your Strava Client Secret: "
read -r client_secret

export TH_STRAVA_CLIENT_ID="$client_id"
export TH_STRAVA_CLIENT_SECRET="$client_secret"

# 3. log in
uv run tileharvester auth

# 4. build your history
uv run tileharvester backfill

# 5. test it
uv run tileharvester sync --once