# lmloop for Android

A thin native shell around the same dashboard as the web app (`web/static/`)
-- the WebView renders it directly, so every screen you see is the PWA
itself. Native code adds three things a browser tab can't: first-run setup,
a foreground-service "live activity" style notification while a run is
being watched, and a background check for finished runs while the app is
fully closed (WorkManager, not Firebase -- see the project epic for why).

Modeled on `~/git/jlevangi/personal-health-collector`'s Gradle/signing/CI
setup: same Kotlin DSL, same env-var-driven release signing, same release
workflow shape.

## Getting started

The setup screen asks for only your server's URL, confirmed with an
unauthenticated `GET /health`. That's enough to use the app: once loaded,
the WebView logs in exactly like a browser tab would -- OIDC, a
trusted-proxy header, or nothing, whatever your `LMLOOP_WEB_AUTH_MODE` is.
No device token is needed to use the dashboard or run the Live Activity
notification: the app reuses your active login session automatically.

A device token (tap the ⚙ in the top-right corner any time) is **optional**
and only needed for notifications while the app is fully closed if your
deployment does not maintain a long-lived web session. To configure one,
generate it on your server and add it to `LMLOOP_WEB_DEVICE_TOKENS` (see
`web/deploy/web.env.example`):

```sh
python3 -c "import secrets; print(secrets.token_urlsafe(32))"
```

then restart `lmloop-web` and paste the token into the app's settings
screen. It's read-only by construction (see `web/device_auth.py`): it can
never start, stop, pause, archive, delete, or open a PR on a run, however
it's configured.

## Building locally

Requires **JDK 21** (Java/Kotlin bytecode remains targeted at Java 17) and **Android SDK 37**. The app still targets API 36 and supports API 28+. The
`android/gradle/libs.versions.toml` pins `compileSdk`, `minSdk`, and
`targetSdk` — nothing in the project names a different level.

```sh
cd android
./gradlew testDebugUnitTest lintDebug assembleDebug
```

`assembleRelease` additionally needs `ANDROID_KEYSTORE_PATH`,
`ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`,
`ANDROID_VERSION_NAME`, and `ANDROID_VERSION_CODE` set -- see below.

## Generating a release keystore

Do this **once**. Obtainium (and any other update mechanism) needs every
release signed with the *same* key forever; losing this keystore means every
future release is a new, unrelated app as far as installed devices are
concerned.

```sh
keytool -genkeypair -v \
  -keystore lmloop-android-release.jks \
  -alias lmloop-android \
  -keyalg RSA -keysize 2048 \
  -validity 10000
```

Keep `lmloop-android-release.jks` somewhere durable and *off* this repo --
`.gitignore` already excludes `*.jks`. Losing it is unrecoverable; back it up
before anything else.

## GitHub Actions setup

Add these four repository secrets (Settings → Secrets and variables →
Actions):

| Secret | Value |
|---|---|
| `ANDROID_KEYSTORE_BASE64` | `base64 -w0 lmloop-android-release.jks` |
| `ANDROID_KEYSTORE_PASSWORD` | the keystore password you chose above |
| `ANDROID_KEY_ALIAS` | `lmloop-android` (or whatever alias you used) |
| `ANDROID_KEY_PASSWORD` | the key password you chose above |

`android-ci.yml` runs on PRs and pushes to `main` that touch `android/**`
and needs no secrets. `android-release.yml` triggers on tags matching `android-v*`
(strict `android-vMAJOR.MINOR.PATCH`) and publishes a signed, verified APK
plus checksum as a GitHub Release:

```sh
git tag android-v1.0.0
git push origin android-v1.0.0
```

## Obtainium

[<img src="https://raw.githubusercontent.com/ImranR98/Obtainium/main/assets/graphics/badge_obtainium.png" alt="Get it on Obtainium" height="60">](https://apps.obtainium.imranr.dev/redirect?r=obtainium://add/https://github.com/jlevangi/lmloop)

Obtainium tracks a GitHub repo's Releases. The badge above adds this repo by
URL directly; either way, set its **release tag filter** to a regex so it
only ever considers Android release tags, never anything else this repo
might one day tag:

```
^android-v
```

Obtainium detects an update when the release's derived `versionCode`
(`major*1_000_000 + minor*1_000 + patch`, computed by the release workflow
from the tag, not chosen by hand) increases -- always true for an increasing
semver tag.

## Testing

- `./gradlew testDebugUnitTest` -- pure-JVM logic: route parsing, JSON
  parsing against a realistic server payload, notification text, and the
  WorkManager scheduling contract (`app/src/test/`).
- `./gradlew connectedDebugAndroidTest` -- `ServerConfigStore`'s
  AndroidKeyStore-backed encryption, which only exists on a real or emulated
  Android runtime (`app/src/androidTest/`).

What isn't (and can't easily be) covered by either: real notification
behavior in the tray, Doze/battery-optimization interaction with the
foreground service and the periodic worker, and the OIDC login flow inside
the WebView. Those need a real device and a real lmloop server -- walk
through setup, watch a real run, close the app and wait for a real
~15-minute poll cycle with the device idle.
