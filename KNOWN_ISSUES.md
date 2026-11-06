# Known issues

## Behaviour to expect

- **Sound doesn't resume after Disconnect → Connect in apps that were already playing.** When the earbuds disconnect, their audio output disappears from Windows, and the app that was playing stays attached to it. Reload the page or restart that app (seen with YouTube in a browser). Anything started afterwards plays normally.
- **The case only reports its battery while an earbud is inside it with the lid open.** Otherwise the app shows the last reading, greyed out, with its age.
- **Low latency turns off in the earbuds whenever they disconnect.** The app turns it back on when they reconnect.
- **Changing the audio codec restarts the earbuds.** The app asks first. Windows doesn't normally use LDAC, so that setting mostly matters for your phone.
- **SmartScreen warns about the installer**, because it isn't code-signed.
- **The installer is only available in English.** The app itself follows your Windows language.

## Not tested yet

- **Other models.** Only the Nothing Ear (a) has been tested. Everything else relies on data from the ear-web project, and the app shows a warning. Controls may not match, or may not work.
- **On the Ear (a):** changing the codec, switching dual connection on or off, and disconnecting another device from the list.
- **Screen scaling above 100%, multiple monitors, and taskbars not at the bottom of the screen.** The panel opens in the bottom-right of the primary screen.

## Limitations

- Models with a different EQ command (CMF Buds, Buds 2, Buds 2a, Buds 2 Plus, Buds Pro 2, Clip Pro, Headphone Pro) don't show EQ presets yet, as the correct values aren't known.
- Not all features of the Nothing X app are here, such as firmware updates, whose process isn't public.
- If you rename your earbuds to something without "Nothing" or "CMF" before the app first sees them, it may not find them.
- Translations other than Portuguese haven't been reviewed by native speakers.
- There are no automatic updates yet: download new versions from the Releases page.

## Unconfirmed

- During testing, the earbuds once dropped from the PC shortly after a reconnect, with no obvious cause. It didn't happen when repeated. If it happens to you, please [open an issue](https://github.com/noiab3d/nothing-windows-ui/issues).
