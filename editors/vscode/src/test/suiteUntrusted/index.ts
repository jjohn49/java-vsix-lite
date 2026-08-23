// Mocha entry point for the UNTRUSTED-workspace suite, run inside a SEPARATE
// extension host instance than `../suite/index.ts` (see `../runTest.ts`) --
// that instance is launched with Workspace Trust enabled and the fixture
// folder untrusted, the opposite of the trusted suite's setup.
import * as path from "path";
import * as fs from "fs";
import Mocha from "mocha";

export function run(): Promise<void> {
  const mocha = new Mocha({ ui: "tdd", color: true, timeout: 60_000 });
  const suiteDir = __dirname;
  for (const file of fs.readdirSync(suiteDir)) {
    if (file.endsWith(".test.js")) {
      mocha.addFile(path.resolve(suiteDir, file));
    }
  }
  return new Promise((resolve, reject) => {
    mocha.run((failures) => {
      if (failures > 0) {
        reject(new Error(`${failures} integration test(s) failed`));
      } else {
        resolve();
      }
    });
  });
}
