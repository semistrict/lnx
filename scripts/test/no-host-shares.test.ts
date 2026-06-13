import { test } from "bun:test";
import { runScript } from "./run-script";

test("no-host-shares", async () => runScript("no-host-shares"), 180_000);
