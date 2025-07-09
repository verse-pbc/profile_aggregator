import { writeFile } from 'fs/promises';
import { Buffer } from 'buffer';

const PROXY_URL = process.env.PROXY_URL || 'http://127.0.0.1:8080';
const PROXY_SECRET = process.env.IMAGE_PROXY_SECRET || 'default-proxy-secret';

const testUrls = [
  'https://evocomp.co.uk/PBTC/polarbear.png',
  'https://i.imgur.com/2j6dMnc.jpg',
  'https://nostr.build/i/090b3e1e19c7449271ee4a8c367d15cf20065e87fb2e5321b31238a997554269.jpg'
];

interface TestResult {
  url: string;
  success: boolean;
  status?: number;
  contentType?: string;
  contentLength?: string;
  error?: string;
  savedAs?: string;
}

async function testProxyImage(imageUrl: string): Promise<TestResult> {
  console.log(`\n${'='.repeat(60)}`);
  console.log(`Testing: ${imageUrl}`);
  
  const proxyUrl = `${PROXY_URL}/proxy-image?url=${encodeURIComponent(imageUrl)}&secret=${PROXY_SECRET}`;
  console.log(`Proxy URL: ${proxyUrl}`);
  
  const result: TestResult = { url: imageUrl, success: false };
  
  try {
    const startTime = Date.now();
    const response = await fetch(proxyUrl);
    const duration = Date.now() - startTime;
    
    result.status = response.status;
    result.contentType = response.headers.get('content-type') || undefined;
    result.contentLength = response.headers.get('content-length') || undefined;
    
    console.log(`Status: ${response.status}`);
    console.log(`Duration: ${duration}ms`);
    console.log(`Content-Type: ${result.contentType || 'not specified'}`);
    console.log(`Content-Length: ${result.contentLength || 'not specified'} bytes`);
    console.log(`Cache-Control: ${response.headers.get('cache-control') || 'not specified'}`);
    
    if (response.ok) {
      const buffer = await response.arrayBuffer();
      const ext = result.contentType?.split('/')[1]?.split(';')[0] || 'bin';
      const filename = `test-proxy-${Date.now()}.${ext}`;
      await writeFile(filename, Buffer.from(buffer));
      
      result.success = true;
      result.savedAs = filename;
      console.log(`✅ Success! Saved as ${filename} (${buffer.byteLength} bytes)`);
    } else {
      const text = await response.text();
      result.error = text;
      console.log(`❌ Error: ${text}`);
    }
  } catch (error) {
    if (error instanceof Error) {
      result.error = error.message;
      console.log(`❌ Failed: ${error.message}`);
      if ('cause' in error) {
        console.log(`   Cause: ${error.cause}`);
      }
    } else {
      result.error = String(error);
      console.log(`❌ Failed: ${result.error}`);
    }
  }
  
  return result;
}

async function testInvalidRequests() {
  console.log('\n🔒 Testing Security & Validation');
  console.log('='.repeat(60));
  
  const invalidTests = [
    {
      name: 'Invalid secret',
      url: `${PROXY_URL}/proxy-image?url=${encodeURIComponent('https://example.com/test.png')}&secret=wrong-secret`
    },
    {
      name: 'Missing secret',
      url: `${PROXY_URL}/proxy-image?url=${encodeURIComponent('https://example.com/test.png')}`
    },
    {
      name: 'Invalid URL',
      url: `${PROXY_URL}/proxy-image?url=not-a-url&secret=${PROXY_SECRET}`
    },
    {
      name: 'Blocked domain',
      url: `${PROXY_URL}/proxy-image?url=${encodeURIComponent('https://blocked-domain.com/test.png')}&secret=${PROXY_SECRET}`
    },
    {
      name: 'Local IP (SSRF attempt)',
      url: `${PROXY_URL}/proxy-image?url=${encodeURIComponent('http://127.0.0.1/test.png')}&secret=${PROXY_SECRET}`
    }
  ];
  
  for (const test of invalidTests) {
    console.log(`\nTesting ${test.name}...`);
    try {
      const response = await fetch(test.url);
      const text = await response.text();
      console.log(`  Status: ${response.status} - ${text}`);
    } catch (error) {
      if (error instanceof Error) {
        console.log(`  Error: ${error.message}`);
        if ('cause' in error) {
          console.log(`  Cause: ${error.cause}`);
        }
      } else {
        console.log(`  Error: ${String(error)}`);
      }
    }
  }
}

async function main() {
  console.log('🖼️  Testing Image Proxy Endpoint');
  console.log('================================');
  console.log(`Proxy URL: ${PROXY_URL}`);
  console.log(`Secret: ${PROXY_SECRET === 'default-proxy-secret' ? 'default-proxy-secret (⚠️  using default)' : '***'}`);
  
  // Test invalid requests first
  await testInvalidRequests();
  
  // Test valid image URLs
  console.log('\n\n📸 Testing Valid Image URLs');
  console.log('='.repeat(60));
  
  const results: TestResult[] = [];
  
  for (const url of testUrls) {
    const result = await testProxyImage(url);
    results.push(result);
  }
  
  // Summary
  console.log('\n\n📊 Summary');
  console.log('='.repeat(60));
  
  const successful = results.filter(r => r.success).length;
  const failed = results.filter(r => !r.success).length;
  
  console.log(`Total tests: ${results.length}`);
  console.log(`✅ Successful: ${successful}`);
  console.log(`❌ Failed: ${failed}`);
  
  if (failed > 0) {
    console.log('\nFailed URLs:');
    results.filter(r => !r.success).forEach(r => {
      console.log(`  - ${r.url}`);
      console.log(`    Status: ${r.status || 'N/A'}, Error: ${r.error || 'Unknown'}`);
    });
  }
  
  console.log('\n✨ Test complete!');
}

// Run the test
main().catch(console.error);
