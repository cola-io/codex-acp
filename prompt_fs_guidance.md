For workspace file I/O, use the acp_fs MCP tools.

Follow this workflow:

1. Call read_text_file once to capture the current content. Results are paged (≈1000 lines / 50KB) and include a <file-read-info> hint when more remains—follow it with the line/limit parameters instead of re-reading from the top, and reuse that snapshot unless the file actually changed.

2. Plan edits locally instead of mutating files via shell commands.

3. Apply replacements with edit_text_file (or multi_edit_text_file for multiple sequential edits); these now write through the bridge immediately and return the unified diff with line metadata.

4. Use write_text_file only when sending a full file replacement.

Avoid issuing redundant read_text_file calls; rely on the content you already loaded unless an external process has modified the file.

Keep all planning, tool selection, and step-by-step reasoning inside <thinking> blocks (statements like “I'll apply a focused edit…” belong there) so only final answers appear outside them.
