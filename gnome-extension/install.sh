#!/usr/bin/env bash
# PC Mirror — GNOME Quick Settings eklentisini kurar.
#
#   bash gnome-extension/install.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
UUID="pc-mirror@ales"
SRC="$ROOT/$UUID"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"

if [[ ! -d "$SRC" ]]; then
  echo "Kaynak yok: $SRC" >&2
  exit 1
fi

echo "== PC Mirror QS eklentisi =="
mkdir -p "$DEST"
# Şemayı derle
glib-compile-schemas "$SRC/schemas"
# Dosyaları kopyala (sembolik bağ yerine kopya: şema yolu net olsun)
rsync -a --delete "$SRC/" "$DEST/"
echo "-> Kurulu: $DEST"

if command -v gnome-extensions >/dev/null 2>&1; then
  # Shell oturumu yenilenmeden enable çoğu zaman "does not exist" der (Wayland).
  gnome-extensions enable "$UUID" 2>/dev/null \
    && echo "-> Etkinleştirildi: $UUID" \
    || echo "-> Etkinleştirme oturum yenilemesinden sonra: gnome-extensions enable $UUID"
else
  echo "-> gnome-extensions yok; Extensions uygulamasından elle açın."
fi

echo
echo "Wayland'da yeni eklenti için bir kez oturumdan ÇIKIŞ gerekir"
echo "(ekran kilidi yetmez!). Sistem menüsü → Güç → Çıkış Yap."
echo "Sonra: gnome-extensions enable $UUID"
echo
echo "UYARI: enable() içinde senkron subprocess KULLANILMAZ — Shell açılışını"
echo "kilitler (boş ekran). Monitör listesi menü açılınca asenkron yüklenir."
echo
echo "mirror-host yolu boşsa şunlar denenir:"
echo "  ~/Development/ScreenCast/target/release/mirror-host"
echo "  PATH içindeki mirror-host"
echo "Tercihlerden mutlak yol da verebilirsin."
