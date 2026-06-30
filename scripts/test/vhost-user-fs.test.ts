import { test } from "bun:test";
import { runScript } from "./run-script";

test("vhost-user-fs", async () => runScript("vhost-user-fs"), 300_000);
