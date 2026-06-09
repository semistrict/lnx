import { test } from "bun:test";
import { runScript } from "./run-script";

test("ingress", async () => runScript("ingress"), 240_000);
