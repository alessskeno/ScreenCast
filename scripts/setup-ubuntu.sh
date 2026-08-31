#!/usr/bin/env bash
# PC Mirror — Ubuntu / Debian / Pop!_OS / Linux Mint kurulum betiği.
#
#   bash scripts/setup-ubuntu.sh
#
# Ubuntu 22.04+ varsayılan olarak GNOME + Wayland kullanır; ekran yakalama ve
# sanal ikinci ekran (--extend) bu kurulumda çalışır.
set -euo pipefail

echo "== PC Mirror — Ubuntu/Debian kurulumu =="

# Derleme: gcc + pkg-config + libopus başlıkları (Arch'ın aksine -dev ayrı paket).
#
# Çalıştırma:
#   gstreamer1.0-tools          : gst-launch-1.0 / gst-inspect-1.0
#   plugins-base                : videoconvert / videorate / videoscale
#   plugins-good                : pulsesrc (ses), ximagesrc (X11 yakalama)
#   pipewire eklentisi          : pipewiresrc (Wayland ekran yakalama — ASIL yol)
#   plugins-bad                 : nvh264enc (NVIDIA), vah264enc (Intel/AMD)
#   plugins-ugly                : x264enc (yazılım yedeği)
#   pulseaudio-utils            : pactl (--tv-audio sanal ses çıkışı)
PKGS=(
  build-essential pkg-config libopus-dev
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good
  gstreamer1.0-pipewire gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly
  pulseaudio-utils
)

echo "-> Paket listesi güncelleniyor"
sudo apt-get update -qq
echo "-> Paketler kuruluyor: ${PKGS[*]}"
sudo apt-get install -y "${PKGS[@]}"

# cargo PATH'te görünmeyebilir (rustup ~/.cargo/bin'e kurar; kabuk
# yapılandırması ancak yeni oturumda etkin olur) — dosya olarak da bak.
if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; then
  echo "-> Rust zaten kurulu."
else
  # Ubuntu deposundaki rustc çoğu zaman eskidir; resmî rustup daha güvenli.
  echo "-> Rust bulunamadı, rustup kuruluyor (resmî betik)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# Bu betiğin geri kalanı için PATH'e al.
[ -x "$HOME/.cargo/bin/cargo" ] && export PATH="$HOME/.cargo/bin:$PATH"

echo
echo "== Denetim =="
command -v cargo >/dev/null 2>&1 \
  && echo "  ✓ Rust: $(cargo --version)" || echo "  ✗ Rust EKSİK"
for e in pipewiresrc pulsesrc videoconvert; do
  gst-inspect-1.0 "$e" >/dev/null 2>&1 \
    && echo "  ✓ $e" || echo "  ✗ $e EKSİK"
done
# NOT: `gst-inspect-1.0 ... | grep -q` KULLANMA. grep eşleşmeyi bulunca hemen
# çıkıp boruyu kapatır, gst-inspect SIGPIPE ile ölür ve `pipefail` yüzünden
# koşul BAŞARISIZ sayılır — çalışan kodlayıcılar "yok" görünür (yaşandı).
# Çıktıyı önce değişkene al, grep'i here-string üzerinde çalıştır.
found=""
for e in nvh264enc vah264enc vah264lpenc x264enc; do
  info=$(gst-inspect-1.0 "$e" 2>/dev/null) || continue
  if grep -qE '^[[:space:]]+aud ' <<<"$info"; then
    echo "  ✓ H.264 kodlayıcı: $e"
    found="$e"
  fi
done
[ -n "$found" ] || echo "  ✗ Kullanılabilir H.264 kodlayıcı YOK (gstreamer1.0-plugins-ugly kurulu mu?)"
command -v pactl >/dev/null 2>&1 \
  && echo "  ✓ pactl (--tv-audio için)" || echo "  ✗ pactl EKSİK (pulseaudio-utils)"

case "${XDG_SESSION_TYPE:-}" in
  wayland) echo "  ✓ Wayland oturumu (GNOME ise sanal ekran --extend çalışır)";;
  x11)     echo "  ! X11 oturumu: aynalama çalışır, --extend ÇALIŞMAZ."
           echo "    Oturum açma ekranında dişli simgesinden 'Ubuntu' (Wayland) seçin.";;
  *)       echo "  ! Oturum tipi bilinmiyor: ${XDG_SESSION_TYPE:-yok}";;
esac

echo
echo "Hazır. Derlemek ve çalıştırmak için:"
echo "  cargo build --release"
echo "  ./target/release/mirror-host          # kontrol paneli"
echo "  ./target/release/mirror-host --bind 0.0.0.0:47000   # doğrudan yayın"
