import GObject from 'gi://GObject';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

/**
 * ÖNEMLİ (2026-09-20): enable() içinde GLib.spawn_sync YOK (Shell kilitlenir).
 *
 * QuickToggleMenu ScrollView'ı güvenilir değil (open() max-height ezer, tekerlek
 * ölür). Bu yüzden SAYFA geçişi: ana sayfa kısa kalır; alt seçenekler ayrı sayfada.
 * hide_on_activate=false ile menü kapanmaz.
 */

const MODES = [
    {label: 'Sürücü varsayılanı', value: ''},
    {label: '1080p @ 60', value: '1920x1080@60'},
    {label: '2K @ 60', value: '2560x1440@60'},
    {label: '4K @ 60', value: '3840x2160@60'},
];

const BITRATES = [
    {label: 'Tasarruf (8 Mb/s)', value: 8},
    {label: 'Dengeli (12 Mb/s)', value: 12},
    {label: 'Kalite (25 Mb/s)', value: 25},
];

const FPS_OPTIONS = [30, 60];
const STOP_KILL_MS = 4000;

const MirrorToggle = GObject.registerClass(
class MirrorToggle extends QuickMenuToggle {
    _init() {
        super._init({
            title: 'PC Mirror',
            iconName: 'video-display-symbolic',
            toggleMode: true,
        });
    }
});

export default class PcMirrorExtension extends Extension {
    enable() {
        try {
            this._settings = this.getSettings();
        } catch (e) {
            logError(e, 'pc-mirror getSettings');
            return;
        }

        this._proc = null;
        this._stopping = false;
        this._killSource = 0;
        this._monitorsLoaded = false;
        this._monitorList = [];
        this._restartPending = false;
        this._runningExtend = false;
        this._runningMode = '';
        this._runningOutput = 0;
        this._page = 'main';

        try {
            this._toggle = new MirrorToggle();
            this._toggle.connect('clicked', () => this._onClicked());
            this._toggle.menu.connect('open-state-changed', (_menu, isOpen) => {
                if (isOpen && this._page === 'main' && !this._monitorsLoaded)
                    this._refreshMonitorsAsync();
            });

            this._showMainPage();
            this._settingsChangedId = this._settings.connect('changed', () => {
                this._syncMenuChecks();
                this._updateNavLabels();
                this._updateSubtitle();
            });

            this._indicator = new SystemIndicator();
            this._indicator.quickSettingsItems.push(this._toggle);
            Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);

            this._updateSubtitle();
            log('pc-mirror: enable OK (sayfa menüsü + ayarda yeniden başlat)');
        } catch (e) {
            logError(e, 'pc-mirror enable');
            this.disable();
        }
    }

    disable() {
        try {
            this._stopStream(true);
        } catch (e) {}

        if (this._settingsChangedId && this._settings) {
            try {
                this._settings.disconnect(this._settingsChangedId);
            } catch (e) {}
            this._settingsChangedId = 0;
        }
        this._clearKillTimer();

        if (this._indicator) {
            this._indicator.quickSettingsItems.forEach(i => {
                try {
                    i.destroy();
                } catch (e) {}
            });
            try {
                this._indicator.destroy();
            } catch (e) {}
        }
        this._indicator = null;
        this._toggle = null;
        this._settings = null;
        this._modeItems = null;
        this._bitrateItems = null;
        this._fpsItems = null;
        this._monitorItems = null;
        this._extendItem = null;
        this._mirrorItem = null;
        this._audioItem = null;
        this._tvAudioItem = null;
        this._modeNav = null;
        this._bitrateNav = null;
        this._fpsNav = null;
        this._monitorNav = null;
        this._monitorPlaceholder = null;
    }

    /** Ayar yaz + yayın açıksa yeniden başlat (GSettings sinyaline güvenme). */
    _commit(writeFn) {
        writeFn();
        this._syncMenuChecks();
        this._updateNavLabels();
        this._updateSubtitle();
        if (this._proc && !this._stopping) {
            log('pc-mirror: ayar değişti → yeniden başlat');
            this._restartStream();
        }
    }

    _navItem(label, onActivate) {
        const item = new PopupMenu.PopupMenuItem(label);
        item.hide_on_activate = false;
        item.connect('activate', () => onActivate());
        return item;
    }

    _backItem() {
        return this._navItem('← Geri', () => this._showMainPage());
    }

    _showMainPage() {
        if (!this._toggle)
            return;
        const menu = this._toggle.menu;
        menu.removeAll();
        this._page = 'main';

        this._mirrorItem = new PopupMenu.PopupMenuItem('Aynala');
        this._extendItem = new PopupMenu.PopupMenuItem('Genişlet (2. ekran)');
        this._mirrorItem.hide_on_activate = false;
        this._extendItem.hide_on_activate = false;
        this._mirrorItem.connect('activate', () =>
            this._commit(() => this._settings.set_boolean('extend', false)));
        this._extendItem.connect('activate', () =>
            this._commit(() => this._settings.set_boolean('extend', true)));
        menu.addMenuItem(this._mirrorItem);
        menu.addMenuItem(this._extendItem);
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._modeNav = this._navItem('Çözünürlük', () => this._showModePage());
        this._bitrateNav = this._navItem('Bit hızı', () => this._showBitratePage());
        this._fpsNav = this._navItem('Kare hızı', () => this._showFpsPage());
        this._monitorNav = this._navItem('Monitör (aynalama)', () => this._showMonitorPage());
        menu.addMenuItem(this._modeNav);
        menu.addMenuItem(this._bitrateNav);
        menu.addMenuItem(this._fpsNav);
        menu.addMenuItem(this._monitorNav);

        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._audioItem = new PopupMenu.PopupSwitchMenuItem(
            'Sistem sesi',
            this._settings.get_boolean('audio')
        );
        this._audioItem.connect('toggled', (_item, state) =>
            this._commit(() => this._settings.set_boolean('audio', state)));
        menu.addMenuItem(this._audioItem);

        this._tvAudioItem = new PopupMenu.PopupSwitchMenuItem(
            "Ses yalnız TV'de",
            this._settings.get_boolean('tv-audio')
        );
        this._tvAudioItem.connect('toggled', (_item, state) =>
            this._commit(() => this._settings.set_boolean('tv-audio', state)));
        menu.addMenuItem(this._tvAudioItem);

        this._modeItems = [];
        this._bitrateItems = [];
        this._fpsItems = [];
        this._monitorItems = [];
        this._monitorPlaceholder = null;

        this._syncMenuChecks();
        this._updateNavLabels();
    }

    _showModePage() {
        const menu = this._toggle.menu;
        menu.removeAll();
        this._page = 'mode';
        menu.addMenuItem(this._backItem());
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._modeItems = [];
        for (const m of MODES) {
            const item = new PopupMenu.PopupMenuItem(m.label);
            item.hide_on_activate = false;
            item._modeValue = m.value;
            item.connect('activate', () =>
                this._commit(() => this._settings.set_string('mode', m.value)));
            menu.addMenuItem(item);
            this._modeItems.push(item);
        }
        this._syncMenuChecks();
    }

    _showBitratePage() {
        const menu = this._toggle.menu;
        menu.removeAll();
        this._page = 'bitrate';
        menu.addMenuItem(this._backItem());
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._bitrateItems = [];
        for (const b of BITRATES) {
            const item = new PopupMenu.PopupMenuItem(b.label);
            item.hide_on_activate = false;
            item._bitrateValue = b.value;
            item.connect('activate', () =>
                this._commit(() => this._settings.set_int('bitrate', b.value)));
            menu.addMenuItem(item);
            this._bitrateItems.push(item);
        }
        this._syncMenuChecks();
    }

    _showFpsPage() {
        const menu = this._toggle.menu;
        menu.removeAll();
        this._page = 'fps';
        menu.addMenuItem(this._backItem());
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._fpsItems = [];
        for (const fps of FPS_OPTIONS) {
            const item = new PopupMenu.PopupMenuItem(`${fps} fps`);
            item.hide_on_activate = false;
            item._fpsValue = fps;
            item.connect('activate', () =>
                this._commit(() => this._settings.set_int('fps', fps)));
            menu.addMenuItem(item);
            this._fpsItems.push(item);
        }
        this._syncMenuChecks();
    }

    _showMonitorPage() {
        const menu = this._toggle.menu;
        menu.removeAll();
        this._page = 'monitor';
        menu.addMenuItem(this._backItem());
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._monitorItems = [];
        this._monitorPlaceholder = new PopupMenu.PopupMenuItem('Yükleniyor…', {
            reactive: false,
            can_focus: false,
        });
        this._monitorPlaceholder.sensitive = false;
        menu.addMenuItem(this._monitorPlaceholder);

        const refresh = this._navItem('Listeyi yenile', () => {
            this._monitorsLoaded = false;
            this._refreshMonitorsAsync();
        });
        menu.addMenuItem(refresh);

        if (this._monitorList.length)
            this._replaceMonitorItems(this._monitorList);
        else
            this._refreshMonitorsAsync();
        this._syncMenuChecks();
    }

    _updateNavLabels() {
        if (!this._settings || this._page !== 'main')
            return;
        const mode = this._settings.get_string('mode');
        const m = MODES.find(x => x.value === mode);
        if (this._modeNav)
            this._modeNav.label.text = m ? `Çözünürlük: ${m.label}` : 'Çözünürlük';
        if (this._bitrateNav)
            this._bitrateNav.label.text = `Bit hızı: ${this._settings.get_int('bitrate')} Mb/s`;
        if (this._fpsNav)
            this._fpsNav.label.text = `Kare hızı: ${this._settings.get_int('fps')} fps`;
        if (this._monitorNav) {
            const out = this._settings.get_int('output');
            const found = this._monitorList.find(x => x.index === out);
            this._monitorNav.label.text = found
                ? `Monitör: ${found.connector}`
                : `Monitör: #${out}`;
        }
    }

    _syncMenuChecks() {
        if (!this._settings || !this._toggle)
            return;

        const extend = this._settings.get_boolean('extend');
        this._mirrorItem?.setOrnament(extend ? PopupMenu.Ornament.NONE : PopupMenu.Ornament.DOT);
        this._extendItem?.setOrnament(extend ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);

        const mode = this._settings.get_string('mode');
        for (const item of this._modeItems || [])
            item.setOrnament(item._modeValue === mode ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);

        const bitrate = this._settings.get_int('bitrate');
        for (const item of this._bitrateItems || [])
            item.setOrnament(item._bitrateValue === bitrate ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);

        const fps = this._settings.get_int('fps');
        for (const item of this._fpsItems || [])
            item.setOrnament(item._fpsValue === fps ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);

        const output = this._settings.get_int('output');
        for (const item of this._monitorItems || [])
            item.setOrnament(item._outputIndex === output ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);
    }

    _refreshMonitorsAsync() {
        const bin = this._findBinary();
        if (!bin) {
            this._setMonitorPlaceholder('mirror-host bulunamadı');
            return;
        }
        this._setMonitorPlaceholder('Yükleniyor…');

        let proc;
        try {
            proc = Gio.Subprocess.new(
                [bin, '--list-monitors'],
                Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE
            );
        } catch (e) {
            logError(e, 'pc-mirror list-monitors spawn');
            this._setMonitorPlaceholder('Monitör listesi alınamadı');
            return;
        }

        proc.communicate_utf8_async(null, null, (_p, res) => {
            try {
                const [, stdout] = proc.communicate_utf8_finish(res);
                const text = stdout ?? '';
                const start = text.indexOf('[');
                const end = text.lastIndexOf(']');
                if (start < 0 || end < start) {
                    this._setMonitorPlaceholder('Monitör listesi okunamadı');
                    return;
                }
                this._monitorList = JSON.parse(text.slice(start, end + 1));
                this._monitorsLoaded = true;
                if (this._page === 'monitor')
                    this._replaceMonitorItems(this._monitorList);
                this._updateNavLabels();
                this._syncMenuChecks();
            } catch (e) {
                logError(e, 'pc-mirror list-monitors parse');
                this._setMonitorPlaceholder('Monitör listesi alınamadı');
            }
        });
    }

    _setMonitorPlaceholder(label) {
        this._clearMonitorItems();
        if (this._monitorPlaceholder) {
            this._monitorPlaceholder.label.text = label;
            this._monitorPlaceholder.visible = true;
        }
    }

    _clearMonitorItems() {
        for (const item of this._monitorItems || []) {
            try {
                item.destroy();
            } catch (e) {}
        }
        this._monitorItems = [];
    }

    _replaceMonitorItems(list) {
        this._clearMonitorItems();
        if (this._page !== 'monitor' || !this._toggle)
            return;

        if (list.length === 0) {
            this._setMonitorPlaceholder('Monitör yok');
            return;
        }

        if (this._monitorPlaceholder)
            this._monitorPlaceholder.visible = false;

        const menu = this._toggle.menu;
        const items = menu._getMenuItems();
        let pos = items.indexOf(this._monitorPlaceholder);
        if (pos < 0)
            pos = 1;

        for (let i = 0; i < list.length; i++) {
            const m = list[i];
            const label = `${m.connector} — ${m.width}x${m.height}${m.primary ? ' (birincil)' : ''}`;
            const item = new PopupMenu.PopupMenuItem(label);
            item.hide_on_activate = false;
            item._outputIndex = m.index;
            item.connect('activate', () =>
                this._commit(() => this._settings.set_int('output', m.index)));
            menu.addMenuItem(item, pos + 1 + i);
            this._monitorItems.push(item);
        }
    }

    _findBinary() {
        if (!this._settings)
            return null;
        const custom = (this._settings.get_string('binary-path') || '').trim();
        if (custom && GLib.file_test(custom, GLib.FileTest.IS_EXECUTABLE))
            return custom;

        const home = GLib.get_home_dir();
        const candidates = [
            `${home}/Development/ScreenCast/target/release/mirror-host`,
            `${home}/.local/bin/mirror-host`,
            '/usr/local/bin/mirror-host',
        ];
        for (const c of candidates) {
            if (GLib.file_test(c, GLib.FileTest.IS_EXECUTABLE))
                return c;
        }
        return GLib.find_program_in_path('mirror-host');
    }

    _buildArgv(bin) {
        const port = this._settings.get_int('port');
        const bitrate = this._settings.get_int('bitrate');
        const fps = this._settings.get_int('fps');
        const argv = [
            bin,
            '--managed',
            '--bind', `0.0.0.0:${port}`,
            '--bitrate', String(bitrate),
            '--fps', String(fps),
        ];
        if (this._settings.get_boolean('extend')) {
            argv.push('--extend');
            const mode = this._settings.get_string('mode');
            if (mode)
                argv.push('--mode', mode);
        } else {
            argv.push('--output', String(this._settings.get_int('output')));
        }
        if (!this._settings.get_boolean('audio'))
            argv.push('--no-audio');
        else if (this._settings.get_boolean('tv-audio'))
            argv.push('--tv-audio');
        return argv;
    }

    _onClicked() {
        if (this._proc)
            this._stopStream(false);
        else
            this._startStream();
    }

    _restartStream() {
        if (this._restartPending)
            return;
        this._restartPending = true;
        this._updateSubtitle();
        this._stopStream(false);

        let tries = 0;
        const tick = () => {
            tries++;
            if (this._proc && tries < 40) {
                GLib.timeout_add(GLib.PRIORITY_DEFAULT, 150, () => {
                    tick();
                    return GLib.SOURCE_REMOVE;
                });
                return;
            }
            this._restartPending = false;
            if (!this._proc) {
                this._startStream();
            } else {
                try {
                    this._proc.force_exit();
                } catch (e) {}
                GLib.timeout_add(GLib.PRIORITY_DEFAULT, 400, () => {
                    this._proc = null;
                    this._restartPending = false;
                    this._startStream();
                    return GLib.SOURCE_REMOVE;
                });
            }
        };
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            tick();
            return GLib.SOURCE_REMOVE;
        });
    }

    _startStream() {
        const bin = this._findBinary();
        if (!bin) {
            Main.notify('PC Mirror', 'mirror-host bulunamadı. Tercihlerden yolu ayarlayın.');
            this._toggle.checked = false;
            return;
        }

        if (!this._proc)
            this._killStaleManaged();

        const argv = this._buildArgv(bin);
        log(`pc-mirror: başlat ${argv.join(' ')}`);
        const cwd = GLib.path_get_dirname(bin);

        try {
            const launcher = new Gio.SubprocessLauncher({
                flags: Gio.SubprocessFlags.STDIN_PIPE |
                    Gio.SubprocessFlags.STDOUT_SILENCE |
                    Gio.SubprocessFlags.STDERR_SILENCE,
            });
            launcher.set_cwd(cwd);
            this._proc = launcher.spawnv(argv);
        } catch (e) {
            logError(e, 'pc-mirror start');
            Main.notify('PC Mirror', e.message ?? String(e));
            this._proc = null;
            this._toggle.checked = false;
            this._updateSubtitle();
            return;
        }

        this._stopping = false;
        this._runningExtend = this._settings.get_boolean('extend');
        this._runningMode = this._settings.get_string('mode') || '';
        this._runningOutput = this._settings.get_int('output');
        this._toggle.checked = true;
        this._updateSubtitle();
        this._watchExit();
    }

    _killStaleManaged() {
        try {
            Gio.Subprocess.new(
                ['pkill', '-f', 'mirror-host --managed'],
                Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_SILENCE
            );
        } catch (e) {}
    }

    _watchExit() {
        if (!this._proc)
            return;
        const proc = this._proc;
        proc.wait_async(null, (_p, res) => {
            try {
                proc.wait_finish(res);
            } catch (e) {}
            if (this._proc === proc) {
                this._proc = null;
                this._stopping = false;
                this._clearKillTimer();
                if (this._toggle)
                    this._toggle.checked = false;
                this._updateSubtitle();
            }
        });
    }

    _stopStream(force) {
        if (!this._proc) {
            if (this._toggle)
                this._toggle.checked = false;
            this._updateSubtitle();
            return;
        }
        this._stopping = true;
        this._updateSubtitle();

        try {
            const stdin = this._proc.get_stdin_pipe();
            if (stdin)
                stdin.close(null);
        } catch (e) {
            try {
                this._proc.force_exit();
            } catch (e2) {}
        }

        this._clearKillTimer();
        this._killSource = GLib.timeout_add(GLib.PRIORITY_DEFAULT, STOP_KILL_MS, () => {
            this._killSource = 0;
            if (this._proc) {
                try {
                    this._proc.force_exit();
                } catch (e) {}
            }
            return GLib.SOURCE_REMOVE;
        });

        if (force && this._proc) {
            try {
                this._proc.force_exit();
            } catch (e) {}
        }
    }

    _clearKillTimer() {
        if (this._killSource) {
            GLib.source_remove(this._killSource);
            this._killSource = 0;
        }
    }

    _updateSubtitle() {
        if (!this._toggle)
            return;
        let text = 'Kapalı';
        if (this._restartPending) {
            text = 'Uygulanıyor…';
        } else if (this._stopping) {
            text = 'Durduruluyor…';
        } else if (this._proc) {
            if (this._runningExtend) {
                const mode = this._runningMode || 'varsayılan';
                const short = mode.includes('3840') ? '4K'
                    : mode.includes('2560') ? '2K'
                    : mode.includes('1920') ? '1080p'
                    : mode;
                text = `Genişlet ${short}`;
            } else {
                text = `Aynalama #${this._runningOutput}`;
            }
        }
        this._toggle.subtitle = text;
        try {
            this._toggle.menu.setHeader('video-display-symbolic', 'PC Mirror', text);
        } catch (e) {}
        this._toggle.checked = !!this._proc && !this._stopping && !this._restartPending;
    }
}
