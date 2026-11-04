import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { captureCarousel, normalizePostUrl } from './instagram-screenshotter.mjs';

function fixture({blocked=false,initial=1}={}) {
  return `<style>body{margin:0} main{padding:20px} #carousel{position:relative;width:360px;height:480px;overflow:hidden}img{width:360px;height:480px} #modal{position:fixed;inset:0;background:white;z-index:10}#modal[hidden]{display:none}</style>
  <main><article><div id="carousel"><img id="photo"><button aria-label="Previous" onclick="step(-1)">Previous</button><button aria-label="Next" onclick="step(1)">Next</button></div></article></main>
  <div id="modal">See this post in the app ${blocked?'':'<button aria-label="Close" onclick="document.querySelector(\'#modal\').hidden=true">Close</button>'}</div>
  <script>let index=${initial};const slides=Array.from({length:5},(_,i)=>'data:image/svg+xml,'+encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="600" height="800"><rect width="600" height="800" fill="'+['red','green','blue','orange','purple'][i]+'"/><text x="10" y="100">Catalog '+i+'</text></svg>'));function step(d){index+=d;document.querySelector('#photo').src=slides[index];document.querySelector('[aria-label=Previous]').hidden=index===0;document.querySelector('[aria-label=Next]').hidden=index===4;}step(0);</script>`;
}

test('normalizes only Instagram photo-post URLs',()=>{
  assert.equal(normalizePostUrl('https://www.instagram.com/artist/p/ABC_123/?img_index=3'),'https://www.instagram.com/p/ABC_123/');
  assert.throws(()=>normalizePostUrl('https://other.example/p/ABC/'));
  assert.throws(()=>normalizePostUrl('https://www.instagram.com/accounts/login/'));
});

for (const scenario of [{name:'rewinds, closes popup, and captures every slide',max:20,status:'complete',count:5},
  {name:'records a configured cap as partial',max:2,status:'partial',count:2},
  {name:'does not screenshot through an undismissable prompt',max:20,status:'blocked',count:0,blocked:true}]) {
  test(scenario.name,async()=>{
    const browser=await chromium.launch({headless:true});
    const output=await mkdtemp(join(tmpdir(),'comi-instagram-test-'));
    try {
      const page=await browser.newPage({viewport:{width:1200,height:1000},deviceScaleFactor:2});
      await page.setContent(fixture({blocked:scenario.blocked}));
      await page.waitForFunction(()=>document.querySelector('#photo').complete);
      const result=await captureCarousel(page,{output,sourceUrl:'https://www.instagram.com/p/TEST/',maxSlides:scenario.max});
      assert.equal(result.status,scenario.status); assert.equal(result.slides.length,scenario.count);
      if (scenario.count) {assert.equal(result.popup_dismissals,1);assert.equal(new Set(result.slides.map(s=>s.sha256)).size,scenario.count);}
      assert.equal(JSON.parse(await readFile(join(output,'manifest.json'),'utf8')).status,scenario.status);
    }finally{await browser.close();await rm(output,{recursive:true,force:true});}
  });
}
