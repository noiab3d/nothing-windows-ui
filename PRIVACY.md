# Privacy

**Nothing Windows UI collects no personal data.**

- No analytics, telemetry, ads, or account.
- The app contains no network code: it doesn't send anything to the author or anyone else. You can check this in the source code.
- It only talks to your earbuds, over Bluetooth, on your own PC.

## What is stored, on your PC only

A small settings file:

- The name and Bluetooth address of your earbuds, and their model
- The case's last battery reading
- Your preferences: theme, language, notifications, low latency, start with Windows

It's at `%APPDATA%\local.nothingwindowsui.app\config.json`. Delete the file to erase it.

"Start with Windows" also adds an entry to your user's Windows startup list (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`), which you can turn off in the app.

## Other software involved

The app's window is drawn by Microsoft Edge WebView2, which is part of Windows 11. If it's missing, the installer downloads it from Microsoft. WebView2 is Microsoft's software, under Microsoft's own terms.

The app can also open the Windows Bluetooth settings, and notifications go through Windows as usual.
