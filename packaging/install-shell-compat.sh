#!/usr/bin/env bash
# Install only the lightweight desktop compatibility entry points.
set -euo pipefail
compat_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/compat"
compat_bin="${HOME}/.local/bin"
compat_data="${XDG_DATA_HOME:-${HOME}/.local/share}"
compat_binary="${compat_bin}/rust-connect"
[[ -x "${compat_binary}" ]] || { echo 'Install rust-connect with install-user-service.sh first.' >&2; exit 1; }
compat_service="${compat_data}/dbus-1/services/org.kde.kdeconnect.service"
if [[ -e "${compat_service}" ]] && ! head -n 1 "${compat_service}" |     cmp -s - <(head -n 1 "${compat_root}/org.kde.kdeconnect.service.in"); then
    echo "Refusing to overwrite existing ${compat_service}" >&2
    exit 1
fi
mkdir -p "${compat_bin}" "${compat_data}/dbus-1/services"
for compat_name in kdeconnectd kdeconnect-app; do
    compat_target="${compat_bin}/${compat_name}"
    if [[ -e "${compat_target}" ]] && ! cmp -s "${compat_root}/${compat_name}" "${compat_target}"; then
        echo "Refusing to overwrite existing ${compat_target}" >&2
        exit 1
    fi
done
for compat_name in kdeconnectd kdeconnect-app; do
    install -m 0755 "${compat_root}/${compat_name}" "${compat_bin}/${compat_name}"
done
python3 - "${compat_root}/org.kde.kdeconnect.service.in" "${compat_data}/dbus-1/services/org.kde.kdeconnect.service" "${compat_binary}" <<'PY'
from pathlib import Path
import sys
# D-Bus Exec follows desktop-entry command quoting; preserve spaces in HOME.
path=sys.argv[3].replace('\\','\\\\').replace('"','\\"').replace('`','\\`').replace('$','\\$')
Path(sys.argv[2]).write_text(Path(sys.argv[1]).read_text().replace('@BINARY@','"'+path+'"'))
PY
printf '%s\n' 'Installed shell compatibility launchers. They require no KDE Frameworks.'
