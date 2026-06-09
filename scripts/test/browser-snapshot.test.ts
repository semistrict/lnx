import { test } from "bun:test";
import { runScript } from "./run-script";

test("browser-snapshot", async () => runScript("browser-snapshot"), 1_200_000);
