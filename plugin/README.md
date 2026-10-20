# SonicCraft plugin for Claude Code

The SonicCraft Agent Skill and MCP server: compile declarative audio timelines (TTS speech, music beds, sound
effects) into ducked, loudness-mastered audio with WebVTT/SRT subtitles.

- `skills/soniccraft/`: the Agent Skill, a copy of the repository's `skills/soniccraft` kept in sync by
  `scripts/sync-plugin.sh`
- `.mcp.json`: the MCP server, `soniccraft mcp` on stdio

The MCP server is the SonicCraft CLI, which the plugin does not bundle: install it with Homebrew
(`brew install user-github-me/soniccraft/soniccraft`), Scoop, `cargo install --locked soniccraft` or a release
archive, as the repository README describes.

Source, documentation and license (MIT OR Apache-2.0): https://github.com/user-github-me/soniccraft
