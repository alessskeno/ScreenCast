#!/usr/bin/env bash
# PC Mirror — Arch / CachyOS / Manjaro kurulum betiği.
# Derleme ve çalıştırma için gereken bağımlılıkları denetler, eksikse kurar.
#
#   bash scripts/setup-arch.sh
set -euo pipefail

echo "== PC Mirror — Arch tabanlı sistem kurulumu =="

# Derleme: gcc + pkg-config, ses kodlaması için sistem libopus'u
# (opus crate'i pkg-config ile onu bulur — Windows'taki gibi cmake gerekmez).
#
# Çalıştırma:
#   gstreamer + base  : videoconvert / videorate / videoscale / fdsink
#   good              : pulsesrc (ses) ve ximagesrc (X11 yakalama)
#   pipewire eklentisi: pipewiresrc (Wayland ekran yakalama — ASIL yol)
#   bad               : nvh264enc (NVIDIA) ve vah264enc (Intel/AMD VA-API)
#   ugly              : x264enc (donanım kodlayıcı yoksa yazılım yedeği)
#   libpulse          : pactl (--tv-audio sanal ses çıkışı için)
PKGS=(
  base-devel pkgconf opus
  gstreamer gst-plugins-base gst-plugins-good gst-plugin-pipewire
  gst-plugins-bad gst-plugins-ugly
  libpulse
)

# Yalnız GERÇEKTEN eksik olanları topla.
#
# TUZAK (yaşandı): burada düz `pacman -S --needed ...` çağırmak YANLIŞ. Paket
# kurulu ama depoda daha yeni sürümü varsa pacman onu YÜKSELTMEYE çalışır; bu
# bir "kısmi yükseltme"dir ve sistemdeki bağımlı paketler (gst-libav,
# pipewire-alsa…) tam sürüme sabitli olduğu için işlem
# "breaks dependency ... required by ..." ile reddedilir.
# Arch'ta kural: ya hiç dokunma, ya tam yükselt (-Syu).
missing=()
for p in "${PKGS[@]}"; do
  pacman -Qq "$p" >/dev/null 2>&1 || missing+=("$p")
done

if [ ${#missing[@]} -eq 0 ]; then
  echo "-> Tüm bağımlılıklar zaten kurulu; pacman'e dokunulmuyor."
else
  echo "-> Eksik paketler: ${missing[*]}"
  echo
  echo "   Arch'ta tek tek paket kurmak kısmi yükseltmeye yol açabilir, bu yüzden"
  echo "   kurulum tam sistem yükseltmesiyle birlikte yapılmalı:"
  echo
  echo "     sudo pacman -Syu --needed ${missing[*]}"
  echo
  pending=$(pacman -Qu 2>/dev/null | wc -l)
  [ "$pending" -gt 0 ] && echo "   (Not: sisteminizde $pending paket yükseltme bekliyor.)"
  if [ -t 0 ]; then
    read -r -p "   Şimdi çalıştırılsın mı? [e/H] " yn
    case "$yn" in
      [eE]*) sudo pacman -Syu --needed "${missing[@]}" ;;
      *)     echo "   Atlandı — komutu kendiniz çalıştırıp betiği tekrar deneyin."; exit 1 ;;
    esac
  else
    echo "   Etkileşimsiz kabuk: komutu elle çalıştırın."; exit 1
  fi
fi

# Rust'ı pacman işleminden AYRI tut: resmî rustup betiği sudo istemez, kısmi
# yükseltme riski taşımaz ve her dağıtımda aynı çalışır. cargo PATH'te
# görünmeyebilir (rustup ~/.cargo/bin'e kurar; kabuk yapılandırması yeni
# oturumda etkin olur) — bu yüzden dosya olarak da bakıyoruz.
if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; then
  echo "-> Rust zaten kurulu."
else
  echo "-> Rust bulunamadı, rustup kuruluyor (resmî betik, sudo gerekmez)"
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
[ -n "$found" ] || echo "  ✗ Kullanılabilir H.264 kodlayıcı YOK (gst-plugins-ugly kurulu mu?)"
command -v pactl >/dev/null 2>&1 \
  && echo "  ✓ pactl (--tv-audio için)" || echo "  ✗ pactl EKSİK (libpulse)"

case "${XDG_SESSION_TYPE:-}" in
  wayland) echo "  ✓ Wayland oturumu (GNOME ise sanal ekran --extend çalışır)";;
  x11)     echo "  ! X11 oturumu: aynalama çalışır, --extend ÇALIŞMAZ (GNOME Wayland gerekir)";;
  *)       echo "  ! Oturum tipi bilinmiyor: ${XDG_SESSION_TYPE:-yok}";;
esac

echo
echo "Hazır. Derlemek ve çalıştırmak için:"
echo "  cargo build --release"
echo "  ./target/release/mirror-host          # kontrol paneli"
echo "  ./target/release/mirror-host --bind 0.0.0.0:47000   # doğrudan yayın"
