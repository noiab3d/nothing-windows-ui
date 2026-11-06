# Contributing

Thanks for helping out! The most useful things right now:

1. **Testing on other models.** Use the [model report](https://github.com/noiab3d/nothing-windows-ui/issues/new?template=model_report.md).
2. **Reviewing translations.** They're all in `i18n.json`.
3. **Bug reports.** Check the [known issues](KNOWN_ISSUES.md) first.

## Code

- The interface is `ui.html` and `i18n.json`, in the project root; `rust/sync-ui.js` copies them at build time. The rest of the app is in `rust/src-tauri/src/`.
- `models.json` is generated: change `tools/make_models.py` rather than editing it by hand.
- To preview the UI in a browser without earbuds, serve the folder and open `ui.html?demo=1`. Also try `demo=off`, `demo=bt` and `lang=de`.

## Please don't

- Add network requests, analytics or telemetry: the [privacy promise](PRIVACY.md) is a core part of this project.
- Add Nothing's logos, images or other copyrighted material.

By contributing, you agree that your contribution is licensed under the [GNU GPL v3](LICENSE).
