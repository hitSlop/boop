# Sourced inside the sandbox's build lock. Boat omits ~/.cache from snapshots.
export XDG_CACHE_HOME="$HOME/boop-cache"
mkdir -p "$XDG_CACHE_HOME" "$HOME/.cache"
cache=org.swift.swiftpm
if [ -d "$HOME/.cache/$cache" ] && [ ! -L "$HOME/.cache/$cache" ] && [ ! -e "$XDG_CACHE_HOME/$cache" ]; then
    mv "$HOME/.cache/$cache" "$XDG_CACHE_HOME/$cache"
fi
mkdir -p "$XDG_CACHE_HOME/$cache"
if [ ! -e "$HOME/.cache/$cache" ] && [ ! -L "$HOME/.cache/$cache" ]; then
    ln -s "$XDG_CACHE_HOME/$cache" "$HOME/.cache/$cache"
fi

