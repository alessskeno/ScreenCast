import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

export default class PcMirrorPrefs extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        const settings = this.getSettings();

        const page = new Adw.PreferencesPage({
            title: 'PC Mirror',
            iconName: 'video-display-symbolic',
        });
        const group = new Adw.PreferencesGroup({
            title: 'Host',
            description: 'Boş bırakırsan PATH ve ~/Development/ScreenCast/target/release/mirror-host denenir.',
        });

        const row = new Adw.EntryRow({title: 'mirror-host yolu'});
        settings.bind('binary-path', row, 'text', Gio.SettingsBindFlags.DEFAULT);

        group.add(row);
        page.add(group);
        window.add(page);
    }
}
