import { resolve } from "node:path";
import Mocha from "mocha";

export function run(): Promise<void> {
  const mocha = new Mocha({ ui: "bdd", timeout: 20_000, color: false });
  mocha.addFile(resolve(__dirname, "extension.test.js"));
  return new Promise((ok, fail) => mocha.run((failures) => (failures ? fail(new Error(`${failures} failed`)) : ok())));
}
