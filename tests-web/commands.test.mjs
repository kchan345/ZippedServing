import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import test from "node:test";
import { downloadCommand, powershellLiteral, suggestedOutput } from "../src/web/commands.js";

test("PowerShell quoting preserves literal arguments without interpolation or extra commands", () => {
  const special = "a'b\u2018c\u2019d\u201ae\u201bf $HOME $(Get-Process) ` & ; #";
  const profile = {
    client_path: `C:\\Tools\\${special}\\zipped-file-client.exe`,
    download_folder: `\\\\server\\share\\${special}\\`
  };
  const directory = `sub/${special}/photos & images`;
  const output = `result ${special}`;
  const command = downloadCommand(profile, directory, output, "http://host:8081/?path=old");
  const script = `
    $ErrorActionPreference = 'Stop'
    $data = [Console]::In.ReadToEnd() | ConvertFrom-Json
    $tokens = $null; $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseInput($data.command, [ref]$tokens, [ref]$errors)
    if ($errors.Count -or $ast.EndBlock.Statements.Count -ne 1) { throw 'Invalid command or extra statements' }
    $pipeline = $ast.EndBlock.Statements[0]
    if ($pipeline.PipelineElements.Count -ne 1) { throw 'Unexpected pipeline' }
    $values = @($pipeline.PipelineElements[0].CommandElements | ForEach-Object {
      if ($_ -isnot [System.Management.Automation.Language.StringConstantExpressionAst]) { throw 'Not a constant string' }
      $_.Value
    })
    ConvertTo-Json -InputObject $values -Compress
  `;
  const parsed = JSON.parse(execFileSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command", script], {
    input: JSON.stringify({ command }), encoding: "utf8"
  }));
  assert.equal(parsed[0], profile.client_path);
  assert.equal(parsed[1], "download");
  assert.equal(new URL(parsed[2]).searchParams.get("path"), directory);
  assert.equal(new URL(parsed[2]).origin, "http://host:8081");
  assert.equal(parsed[3], `${profile.download_folder.slice(0, -1)}\\${output}`);
  assert.equal(parsed.length, 4);
});

test("root, spaces, Unicode, drive roots, and output validation", () => {
  assert.equal(suggestedOutput(""), "root-download");
  assert.equal(suggestedOutput("parent/child"), "child-download");
  const profile = { client_path: "C:\\tools\\client.exe", download_folder: "D:\\" };
  assert.equal(downloadCommand(profile, "", "root-download", "https://files.test:8443"),
    "& 'C:\\tools\\client.exe' download 'https://files.test:8443/api/download?path=' 'D:\\root-download'");
  for (const name of ["", "..", "../escape", "sub\\folder", "NUL", "CON.txt", "COM\u00b9", "file:", "tail.", "tail ", "a\nb"]) {
    assert.throws(() => downloadCommand(profile, "", name, "http://host"), undefined, name);
  }
  assert.throws(() => powershellLiteral("line\r\nbreak"));
  assert.ok(downloadCommand(profile, "\u4e2d\u6587", "Unicode \u00e9", "http://host").includes("Unicode \u00e9"));
});
