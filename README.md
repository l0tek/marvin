# Marvin

Lokaler Sprachassistent in Rust mit Wake-Word-Erkennung, optionalem Dialog ueber OpenRouter und Audiozugriff aus einem Docker-Container.

## Zweck

Der Dienst hoert permanent auf ein konfigurierbares Wake-Word, standardmaessig `marvin`. Sobald das Wake-Word erkannt wird, oeffnet der Prozess ein kurzes Fragefenster. In diesem Zeitraum wird gesprochene Sprache als Frage interpretiert und optional an OpenRouter geschickt. Die Antwort kann anschliessend per Text-to-Speech wiedergegeben werden.

## Architektur

Die Anwendung besteht aus drei Ebenen:

1. Container-Ebene
   Der Container bringt die Rust-Binary, `libvosk.so`, das Vosk-Sprachmodell, ALSA-Tools und `espeak-ng` mit.
2. Audio- und Erkennungsebene
   `arecord` liest Rohdaten vom Mikrofon. Vosk verarbeitet den Audiostream fuer Wake-Word und Frageerkennung.
3. Dialog-Ebene
   Nach dem Wake-Word verwaltet `DialogManager` den Fragezustand, baut den Request an OpenRouter und haelt einen kurzen Verlauf fuer Folgefragen.

## Container-Verwaltung

### Relevante Dateien

- `Dockerfile`
  Baut ein Multi-Stage-Image, laedt die passende `libvosk.so` je Architektur und installiert zur Laufzeit ALSA, `aplay` und `espeak-ng`.
- `docker-compose.yml`
  Startet den Container `marvin`, reicht `/dev/snd` durch und setzt die wichtigsten Umgebungsvariablen.

### Container bauen und starten

```bash
docker compose up --build
```

Der Container braucht Audiozugriff auf den Host. Deshalb wird in `docker-compose.yml` das Geraet `/dev/snd` in den Container gemountet.

### Im Hintergrund starten

```bash
docker compose up -d --build
```

### Logs ansehen

```bash
docker compose logs -f marvin
```

Wichtige Logmeldungen sind:

- `WAKE WORD erkannt`
- `Frage erkannt`
- `OpenRouter: ...`
- `TTS Fehler: ...`

### Stoppen und entfernen

```bash
docker compose down
```

### Neustartverhalten

In `docker-compose.yml` ist `restart: unless-stopped` gesetzt. Der Container startet also nach Host-Neustarts oder Abstuerzen automatisch wieder, solange er nicht bewusst gestoppt wurde.

### Architektur und Build

Das `Dockerfile` erkennt `amd64`, `arm64` und `arm` und laedt die passende Vosk-Bibliothek. Dadurch ist das Projekt fuer typische Linux-Systeme und Raspberry-Pi-Setups geeignet.

Beispiel fuer einen Multi-Arch-Build:

```bash
docker buildx create --name multiarch --use
docker buildx inspect --bootstrap
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t <dockerhub-user>/marvin:latest \
  --push \
  .
```

## Deployment auf einen Raspberry Pi 4 (SSH, 192.168.55.85)

**Voraussetzungen auf dem Pi (einmalig)**

```bash
ssh pi@192.168.55.85
curl -sSL https://get.docker.com | sh
sudo apt-get install -y docker-compose-plugin
sudo usermod -aG docker $USER
sudo usermod -aG audio $USER   # Zugriff auf /dev/snd fuer Mikro/Audio
newgrp docker
```

**Code auf den Pi kopieren**

```bash
rsync -av --exclude target . pi@192.168.55.85:~/marvin
```

**Konfiguration setzen**

Auf dem Pi im Projektverzeichnis `~/marvin` eine `.env` anlegen, z.B.:

```bash
WAKE_WORD=marvin
TTS_ENABLED=1
OPENROUTER_API_KEY=sk-...
OPENROUTER_MODEL=gpt-4o-mini
ALSA_DEVICE=hw:1,0        # anpassen nach arecord -l
ALSA_OUTPUT_DEVICE=hw:1,0 # optional
```

**Container bauen und starten (auf dem Pi)**

```bash
cd ~/marvin
docker compose up -d --build
docker compose logs -f marvin
```

**Audio kurz testen**

```bash
arecord -D hw:1,0 -f S16_LE -r 16000 -d 3 test.wav
aplay   -D hw:1,0 test.wav
```

**Alternative: Image auf dem Dev-Rechner bauen und pushen**

```bash
docker buildx create --name multiarch --use
docker buildx inspect --bootstrap
docker buildx build --platform linux/arm64 -t <user>/marvin:latest --push .
```

Danach auf dem Pi `docker pull <user>/marvin:latest` und im `docker-compose.yml` das Image referenzieren (statt `build:`), anschliessend `docker compose up -d`.

## Konfiguration

Die Steuerung laeuft fast vollstaendig ueber Umgebungsvariablen.

### Wake-Word

- `WAKE_WORD`
  Primaeres Wake-Word, Standard `marvin`
- `WAKE_WORD_ALIASES`
  Kommagetrennte Alternativen wie `marwin,marvyn`
- `WAKE_FUZZY_DISTANCE`
  Erlaubte Levenshtein-Distanz fuer unscharfe Treffer
- `WAKE_USE_GRAMMAR`
  Wenn `1`, nutzt Vosk fuer das Wake-Word eine eingeschraenkte Grammatik
- `WAKE_COOLDOWN_MS`
  Sperrzeit nach einer Erkennung, um Mehrfachtrigger zu vermeiden
- `DEBUG_WAKEWORD`
  Aktiviert detaillierte Debug-Ausgaben

### Audio

- `ALSA_DEVICE`
  Bevorzugtes Eingabegeraet fuer `arecord`
- `ALSA_OUTPUT_DEVICE`
  Optionales Ausgabegeraet fuer TTS, sonst wird das aktive Eingabegeraet uebernommen
- `SAMPLE_RATE`
  Standard `16000`

### TTS

- `TTS_ENABLED`
  Aktiviert Sprachausgabe
- `TTS_TEXT`
  Bestaetigung nach Wake-Word
- `TTS_VOICE`
  Stimme fuer `espeak-ng`, Standard `de`
- `TTS_SPEED`
  Sprechgeschwindigkeit
- `TTS_COOLDOWN_MS`
  Mindestabstand zwischen zwei TTS-Ausgaben
- `OPENROUTER_TTS_MAX_CHARS`
  Begrenzt die vorgelesene Antwortlaenge

### OpenRouter

- `OPENROUTER_API_KEY`
  Aktiviert den Dialogmodus, wenn gesetzt
- `OPENROUTER_MODEL`
  Zu verwendendes Modell
- `OPENROUTER_BASE_URL`
  API-Endpunkt
- `OPENROUTER_SITE_URL`
  Optionaler HTTP-Referer
- `OPENROUTER_SITE_NAME`
  Optionaler Titel fuer den Request
- `OPENROUTER_SYSTEM_PROMPT`
  Systemprompt fuer die Antworten
- `QUESTION_WINDOW_MS`
  Dauer des Fragefensters nach dem Wake-Word
- `OPENROUTER_MAX_HISTORY_TURNS`
  Anzahl gemerkter Rueckfragen

## Funktionsweise des Codes

### Einstiegspunkt

`src/main.rs` ist absichtlich klein und delegiert alles an `wakeword::run()`.

### Modul `src/wakeword.rs`

Dieses Modul orchestriert den kompletten Lauf:

1. Liest Konfiguration aus Umgebungsvariablen.
2. Normalisiert Wake-Word und Aliase.
3. Laedt das Vosk-Modell.
4. Erstellt zwei Recognizer:
   einen fuer das Wake-Word und einen fuer Fragen.
5. Ermittelt ein funktionierendes ALSA-Geraet.
6. Startet `arecord` als Subprozess und liest kontinuierlich PCM-Daten.
7. Fuehrt Wake-Word-Erkennung auf Partial- und Final-Ergebnissen aus.
8. Startet nach Treffer das Fragefenster und optional TTS.
9. Leitet erkannte Fragen an `DialogManager` weiter.
10. Gibt Antworten optional wieder per `espeak-ng` und `aplay` aus.

Wichtige technische Details:

- Die Wake-Word-Erkennung arbeitet nicht nur mit exakten Treffern, sondern auch mit unscharfen Vergleichen per Levenshtein-Distanz.
- Umlaute werden normalisiert, damit `Märvin` und `Marvin` robust vergleichbar bleiben.
- Es gibt getrennte Cooldowns fuer Wake-Word und TTS.
- `Ctrl+C` beendet den Hauptloop sauber und stoppt `arecord`.

### Modul `src/dialog.rs`

Dieses Modul kapselt den Dialogzustand:

- `DialogConfig`
  Konfiguration fuer OpenRouter und das Fragefenster
- `DialogManager`
  Steuert, ob das System im Fragefenster oder im fortlaufenden Chat ist
- `DialogOutcome`
  Rueckgabe fuer die Hauptroutine: keine Aktion, Antwort erhalten oder Dialog beendet

Der Ablauf ist:

1. Nach Wake-Word startet `begin_question_window()`.
2. Gesprochene Frage wird ueber Vosk transkribiert.
3. `process_final()` bewertet, ob der erkannte Text lang oder plausibel genug fuer eine Anfrage ist.
4. `ask_openrouter()` baut den HTTP-Request und extrahiert `choices[0].message.content`.
5. Frage und Antwort werden in einem kurzen Verlauf gespeichert.
6. Phrasen wie `danke das wars` beenden den Dialogmodus wieder.

## Laufzeitablauf

Ein typischer Durchlauf sieht so aus:

1. Container startet und laedt Modell sowie Audio-Konfiguration.
2. `arecord` liefert permanent Mono-PCM mit 16 kHz.
3. Vosk erkennt `marvin`.
4. Der Container bestaetigt die Aktivierung optional per TTS.
5. Fuer einige Sekunden wird auf eine Frage gehoert.
6. Die Frage geht an OpenRouter, falls ein API-Key gesetzt ist.
7. Die Antwort erscheint im Log und kann vorgelesen werden.

## Sicherheit und Betrieb

- API-Keys sollten nicht fest im Compose-File eingecheckt werden. Besser sind `.env`, Docker-Secrets oder Host-Umgebungsvariablen.
- Der Container braucht direkten Zugriff auf Audio-Hardware. Das ist fuer lokale Sprachverarbeitung notwendig, erhoeht aber die Kopplung an den Host.
- Das Sprachmodell wird beim Image-Build geladen. Dadurch ist das Runtime-Image sofort einsatzbereit, der Build dauert aber laenger.

## Fehlersuche

### Kein Mikrofon gefunden

Pruefen:

```bash
arecord -l
```

Dann `ALSA_DEVICE` passend setzen, zum Beispiel `hw:1,0`.

### Wake-Word wird schlecht erkannt

- `DEBUG_WAKEWORD=1` aktivieren
- `WAKE_WORD_ALIASES` erweitern
- `WAKE_FUZZY_DISTANCE` vorsichtig erhoehen
- Mikrofon und Sample-Rate pruefen

### Keine Antwort von OpenRouter

Pruefen:

- `OPENROUTER_API_KEY` gesetzt
- Modellname korrekt
- Netzwerkzugriff aus dem Container verfuegbar

## Entwicklung ohne Docker

Prinzipiell laeuft das Projekt auch lokal, sofern folgende Voraussetzungen erfuellt sind:

- Rust Toolchain
- `libvosk.so`
- ALSA-Tools
- `espeak-ng`
- heruntergeladenes Vosk-Modell

Start lokal:

```bash
cargo run --release
```
