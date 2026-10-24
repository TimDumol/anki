// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { chromium } from "playwright";

const cards = JSON.parse(await readFile(process.argv[2], "utf8"));
assert.equal(cards.length, 4);
const server = createServer((_, response) => response.end("<!doctype html><body></body>"));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}/`;
let browser;
try {
    browser = await chromium.launch({ headless: true, executablePath: process.env.ANKI_CONTEXT_CHROMIUM });
    for (const isolatedFlip of [false, true]) {
        const context = await browser.newContext();
        await context.addInitScript(() => {
            Math.random = () => 0;
        });
        const pageFor = async () => {
            const page = await context.newPage();
            await page.goto(origin);
            return page;
        };
        for (
            const card of cards.flatMap((card) => [card, { ...card, front: card.legacy_front, back: card.legacy_back }])
        ) {
            let page = await pageFor();
            const selections = new Set();
            for (let presentation = 0; presentation < 6; presentation++) {
                await page.setContent(`<style>${card.css}</style>${card.front}`);
                assert.equal(
                    await page.locator("body").innerText().then((text) => text.includes("undefined")),
                    false,
                    "production cue missing",
                );
                const front = await selection(page);
                selections.add(JSON.stringify(front));
                if (isolatedFlip) {
                    await page.close();
                    page = await pageFor();
                }
                await page.setContent(`<style>${card.css}</style>${card.back}`);
                assert.deepEqual(await selection(page), front, `card ${card.id}: answer changed context`);
                const answer = await page.locator(".solution, .selected-answer, .context-target").first().textContent();
                const dictRecognition = await page.locator(".study.recognition").count();
                assert.equal(answer, dictRecognition ? "to eat" : "comer", "answer target missing or changed");
            }
            assert.equal(selections.size, 6, `card ${card.id}: no new context each presentation`);
            await page.close();
        }
        await context.close();
    }
    const context = await browser.newContext();
    await context.addInitScript(() => {
        for (const name of ["localStorage", "sessionStorage"]) {
            Object.defineProperty(window, name, {
                get() {
                    throw new Error("storage unavailable");
                },
            });
        }
    });
    const page = await context.newPage();
    await page.goto(origin);
    for (const card of cards) {
        await page.setContent(card.front);
        const front = await selection(page);
        await page.setContent(card.back);
        assert.deepEqual(await selection(page), front, `card ${card.id}: storage fallback changed context`);
    }
    await context.close();
    console.log(
        JSON.stringify({
            cards: 4,
            presentations_per_card: 6,
            same_page_flip: "passed",
            separate_page_flip: "passed",
            legacy_context_fields: "passed",
            production_cue_and_answer: "passed",
            storage_unavailable_fallback: "passed",
            actual_desktop_android_acceptance: "pending",
        }),
    );
} finally {
    if (browser) { await browser.close(); }
    await new Promise((resolve) => server.close(resolve));
}

async function selection(page) {
    return page.evaluate(() => {
        const root = document.querySelector(".study, .readest-card");
        if (!root) { throw new Error("Rendered card root missing"); }
        return [
            root.querySelector(".before, .context-before").textContent,
            root.querySelector(".after, .context-after").textContent,
        ];
    });
}
