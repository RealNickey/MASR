# MASR local MCP server

MASR can expose a loopback-only Streamable HTTP MCP server for local clients. It is disabled by default and is enabled from Settings → Advanced → Integrations.

## Connection

Choose a port (default `8787`), enable the server, then use “Show connection” to copy the endpoint and bearer token. The endpoint is:

```text
http://127.0.0.1:<port>/mcp
```

The token is stored in the OS credential vault when available. Rotate it after sharing a configuration or when access should be revoked. Requests must include `Authorization: Bearer <token>`; browser origins are restricted to the loopback endpoint.

## Tools

- `masr_list_downloaded_models` — list installed transcription models.
- `masr_list_recordings` and `masr_get_recording` — read transcript and summary data.
- `masr_search_memory` — search meeting memory using Gemini embeddings and the local SQLite vector index.
- `masr_start_transcription` and `masr_get_transcription_job` — run a local audio file through a downloaded model and poll the job.
- `masr_clear_summary` — remove only a saved summary.
- `masr_delete_recording` — remove a recording, retained audio, transcript, summary, and vectors.

Transcription jobs accept absolute local audio paths and never change the selected MASR model or other settings. The server allows one active job and retains the most recent 32 job records.

## Vector memory

Vector memory is also opt-in. It uses the user-provided Google/Gemini BYOK key from the existing provider settings to call `gemini-embedding-2`; vectors and chunks remain in the local `history.db`. Environment/app-default keys are not used for RAG. If the key is missing, the toggle is rejected and the existing full-transcript meeting Q&A path remains available.

The index contains meeting transcripts and summaries only. “Reindex” refreshes changed content; “Clear index” removes all stored vectors without deleting recordings.

## Client example

Configure a local MCP client with the endpoint and bearer token shown by MASR. For a raw HTTP auth check (the MCP endpoint itself is not a REST API):

```powershell
$headers = @{ Authorization = "Bearer <token>" }
Invoke-WebRequest -Uri "http://127.0.0.1:8787/mcp" -Headers $headers -Method Get
```

Use an MCP client or Inspector for protocol initialization and tool calls; the endpoint is not a REST API.
