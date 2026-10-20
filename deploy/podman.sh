#!/bin/sh
# Run on the Linux Podman host. No compose provider or privileged container needed.
set -eu
umask 077

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=${QKBOT_IMAGE:-localhost/qkbot-r:latest}
name=${QKBOT_CONTAINER:-qkbot}
volume=${QKBOT_VOLUME:-qkbot-data}
mode=${QKBOT_NETWORK:-host}
pod=${QKBOT_POD:-qkbot}
timeout=${QKBOT_STOP_TIMEOUT:-600}
label=io.github.fox-yun.qkbot
for value in "$name" "$volume" "$pod"; do
  case "$value" in ""|*[!a-zA-Z0-9_.-]*) echo "Invalid container/volume/pod name" >&2; exit 2;; esac
done
case "$mode" in host|pod) ;; *) echo "QKBOT_NETWORK must be host or pod" >&2; exit 2;; esac
case "$timeout" in ""|*[!0-9]*) echo "Invalid stop timeout" >&2; exit 2;; esac

owned() {
  owner=$(podman inspect --format '{{index .Config.Labels "io.github.fox-yun.qkbot"}}' "$name")
  [ "$owner" = managed ] || { echo "Container $name was not created by this script" >&2; exit 1; }
}

case "${1:-help}" in
  build)
    revision=$(git -C "$root" rev-parse HEAD 2>/dev/null || printf unknown)
    podman build --file "$root/Containerfile" --tag "$image" \
      --build-arg "VCS_REF=$revision" --build-arg "BUILD_JOBS=${BUILD_JOBS:-1}" \
      --build-arg "CARGO_MIRROR=${CARGO_MIRROR:-}" "$root"
    ;;
  start)
    if podman container exists "$name"; then
      owned
      podman start "$name"
      exit 0
    fi
    podman volume create --ignore "$volume" >/dev/null
    set -- run --detach --name "$name" --label "$label=managed" \
      --stop-timeout "$timeout" --restart unless-stopped \
      --read-only --cap-drop all --security-opt no-new-privileges \
      --volume "$volume:/data" --env "QKBOT_CONTAINER_NETWORK=$mode" \
      --log-driver k8s-file --log-opt max-size=10mb
    if [ "$mode" = host ]; then
      set -- "$@" --network host
    else
      podman pod exists "$pod" || {
        echo "Create pod $pod and attach LLBot to it first; see docs/deployment.md." >&2
        exit 1
      }
      set -- "$@" --pod "$pod"
    fi
    # Optional Podman secrets. Values are never copied into the image/config.
    if [ -n "${QKBOT_API_SECRET:-}" ]; then
      set -- "$@" --secret "$QKBOT_API_SECRET,type=env,target=QKBOT_API_KEY"
    fi
    if [ -n "${QKBOT_MILKY_SECRET:-}" ]; then
      set -- "$@" --secret "$QKBOT_MILKY_SECRET,type=env,target=QKBOT_MILKY_TOKEN"
    fi
    if [ -n "${QKBOT_LLBOT_SECRET:-}" ]; then
      set -- "$@" --secret "$QKBOT_LLBOT_SECRET,type=env,target=QKBOT_LLBOT_TOKEN"
    fi
    if [ -n "${QKBOT_NONEBOT_SECRET:-}" ]; then
      set -- "$@" --secret "$QKBOT_NONEBOT_SECRET,type=env,target=QKBOT_NONEBOT_TOKEN"
    fi
    podman "$@" "$image"
    ;;
  stop)
    owned
    podman stop --time "$timeout" "$name"
    ;;
  status)
    owned
    podman inspect --format '{{.State.Status}} image={{.ImageName}} exit={{.State.ExitCode}}' "$name"
    ;;
  logs)
    owned
    podman logs --tail 100 "$name"
    ;;
  backup)
    owned
    [ "$(podman inspect --format '{{.State.Running}}' "$name")" = false ] || {
      echo "Stop the bot before backup; copying a live SQLite volume is unsafe." >&2
      exit 1
    }
    started=$(podman inspect --format '{{.State.StartedAt}}' "$name")
    output=${2:?Usage: sh deploy/podman.sh backup /path/to/new-backup.tar}
    [ ! -e "$output" ] && [ ! -L "$output" ] || {
      echo "Backup already exists; refusing overwrite" >&2; exit 1;
    }
    # Use the container's actual volume, not a possibly changed caller environment.
    mounted=$(podman inspect --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}' "$name")
    [ -n "$mounted" ] || { echo "Named /data volume missing" >&2; exit 1; }
    # Only publish a completed export. A failed export must not leave a final
    # archive that could later be mistaken for a valid SQLite backup.
    directory=$(dirname -- "$output")
    temporary=$(mktemp "$directory/.qkbot-backup-XXXXXX")
    trap 'rm -f "$temporary"' EXIT HUP INT TERM
    podman volume export --output "$temporary" "$mounted"
    [ "$(podman inspect --format '{{.State.Running}}' "$name")" = false ] \
      && [ "$(podman inspect --format '{{.State.StartedAt}}' "$name")" = "$started" ] || {
      echo "Container started during backup; refusing to publish the archive." >&2; exit 1;
    }
    chmod 600 "$temporary"
    # Hard-link creation also refuses an output created concurrently.
    ln "$temporary" "$output"
    rm -f "$temporary"
    trap - EXIT HUP INT TERM
    ;;
  *)
    echo "Usage: sh deploy/podman.sh build|start|stop|status|logs|backup FILE"
    echo "Defaults: localhost/qkbot-r:latest, container qkbot, volume qkbot-data, Linux host network."
    ;;
esac
