# Compatibility hold for Semgrep 1.180.0, matching agent-toolbox-nix.
# Remove with that consumer once upstream fixes both the ELF rewrite and MCP API.
{ pkgs }:
if pkgs.semgrep.version != "1.180.0" then
  pkgs.semgrep
else
  pkgs.semgrep.override (
    previous:
    {
      semgrep-core = pkgs.semgrep.semgrep-core.overrideAttrs (old: {
        # A relative RUNPATH fits without patchelf splitting the wheel's LOAD segments.
        postFixup = pkgs.lib.replaceStrings [ "$out/bin/libs" ] [ "\\$ORIGIN/libs" ] old.postFixup;
      });
    }
    // pkgs.lib.optionalAttrs (previous ? mcp) {
      # Older package sets call this argument mcp; newer ones already use mcp_1.
      mcp = pkgs.python3Packages.mcp_1;
    }
  )
