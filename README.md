# Wake-Word Spracherkennung (Rust, lokal, Docker)

Dieses Projekt hoert lokal auf ein Wake-Word (Standard: `marvin`) und laeuft in einem Docker-Container.

## Voraussetzungen

- Docker + Docker Compose
- Linux Host mit ALSA (`/dev/snd`)
- Mikrofon am Host

## Start

```bash
docker compose up --build
```

## Konfiguration

Wake-Word anpassen:

```bash
WAKE_WORD="hallo marvin" docker compose up --build
```

OpenRouter fuer Fragen nach Wake-Word aktivieren:

```bash
OPENROUTER_API_KEY="<dein-key>" \
OPENROUTER_MODEL="openai/gpt-4o-mini" \
QUESTION_WINDOW_MS=8000 \
docker compose up --build
```

## Hinweise

- Das Modell wird im Docker-Build heruntergeladen (`vosk-model-small-de-0.15`).
- Bei Erkennung wird `WAKE WORD erkannt` im Log ausgegeben.
- Nach Wake-Word wird ein Fragefenster geoeffnet; erkannte Fragen werden an OpenRouter gesendet.
- Beenden:

```bash
docker compose down
```

## Multi-Arch Buildx (amd64 + arm64)

Beispiel fuer einen Multi-Arch Build und Push (z.B. fuer Laptop + Raspberry Pi 4):

```bash
docker buildx create --name multiarch --use
docker buildx inspect --bootstrap
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t <dockerhub-user>/wakeword-listener:latest \
  --push \
  .
```

Danach kannst du auf dem Zielsystem (z.B. Pi 4) direkt dieses Image verwenden.
