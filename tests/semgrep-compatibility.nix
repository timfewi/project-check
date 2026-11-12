{ pkgs }:
let
  inherit (pkgs) lib;
  core.overrideAttrs = change: change { postFixup = "RUNPATH=$out/bin/libs"; };
  legacy =
    lib.makeOverridable
      (
        { mcp, semgrep-core }:
        {
          version = "1.180.0";
          inherit mcp semgrep-core;
        }
      )
      {
        mcp = "mcp-2";
        semgrep-core = core;
      };
  current =
    lib.makeOverridable
      (
        { mcp_1, semgrep-core }:
        {
          version = "1.180.0";
          inherit mcp_1 semgrep-core;
        }
      )
      {
        mcp_1 = "mcp-1";
        semgrep-core = core;
      };
  repair =
    semgrep:
    import ../packages/semgrep.nix {
      pkgs = {
        inherit lib semgrep;
        python3Packages.mcp_1 = "mcp-1";
      };
    };
  repairedLegacy = repair legacy;
  repairedCurrent = repair current;
in
assert repairedLegacy.mcp == "mcp-1";
assert repairedCurrent.mcp_1 == "mcp-1";
assert repairedLegacy.semgrep-core.postFixup == "RUNPATH=\\$ORIGIN/libs";
assert repairedCurrent.semgrep-core.postFixup == "RUNPATH=\\$ORIGIN/libs";
assert repair { version = "1.181.0"; } == { version = "1.181.0"; };
pkgs.runCommand "semgrep-compatibility" { } ''
  touch "$out"
''
