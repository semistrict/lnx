import { test } from "bun:test";
import { runScript } from "./run-script";

test("privileged-ingress", async () => runScript("privileged-ingress"), 300_000);
