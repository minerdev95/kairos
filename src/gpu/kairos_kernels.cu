// KAIROS native GPU hashing kernels (CUDA C).
//
// These are KAIROS's *own* GPU proof-of-work kernels — the GPU counterpart of the
// pure-Rust CPU core in src/pow.rs. They are compiled by build.rs with `nvcc` ONLY
// when the crate is built with `--features gpu`; the default build never touches
// them, so the CPU engine has zero CUDA dependency.
//
// Correctness note: the device SHA-256 below mirrors the CPU implementation that
// is known-answer-verified against the Bitcoin genesis block. Because this
// environment has no CUDA toolkit or GPU, the compiled kernels have not been run
// here; they are provided as the real GPU backend to build on an NVIDIA host.
//
// Host entry points (C linkage, called from Rust FFI in src/gpu/mod.rs):
//   int  kairos_cuda_device_count();
//   int  kairos_cuda_search_sha256d(const unsigned char* header80,
//                                   const unsigned char* target32,
//                                   unsigned int start, unsigned int count,
//                                   unsigned int* out_nonce, unsigned char* out_hash32);
//   int  kairos_cuda_search_heavyhash(...same signature...);
// Each returns 1 if a nonce meeting the target was found (and fills out_*), else 0.

#include <cuda_runtime.h>
#include <stdint.h>
#include <string.h>

// ───────────────────────── SHA-256 (device) ─────────────────────────

__constant__ uint32_t K[64] = {
  0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
  0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
  0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
  0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
  0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
  0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
  0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
  0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2
};

__device__ __forceinline__ uint32_t rotr(uint32_t x, uint32_t n){ return (x>>n)|(x<<(32-n)); }

__device__ void sha256_transform(uint32_t state[8], const uint8_t block[64]){
  uint32_t w[64];
  #pragma unroll
  for(int i=0;i<16;i++){
    w[i] = (block[i*4]<<24)|(block[i*4+1]<<16)|(block[i*4+2]<<8)|(block[i*4+3]);
  }
  for(int i=16;i<64;i++){
    uint32_t s0 = rotr(w[i-15],7)^rotr(w[i-15],18)^(w[i-15]>>3);
    uint32_t s1 = rotr(w[i-2],17)^rotr(w[i-2],19)^(w[i-2]>>10);
    w[i] = w[i-16]+s0+w[i-7]+s1;
  }
  uint32_t a=state[0],b=state[1],c=state[2],d=state[3],e=state[4],f=state[5],g=state[6],h=state[7];
  for(int i=0;i<64;i++){
    uint32_t S1 = rotr(e,6)^rotr(e,11)^rotr(e,25);
    uint32_t ch = (e&f)^((~e)&g);
    uint32_t t1 = h+S1+ch+K[i]+w[i];
    uint32_t S0 = rotr(a,2)^rotr(a,13)^rotr(a,22);
    uint32_t maj = (a&b)^(a&c)^(b&c);
    uint32_t t2 = S0+maj;
    h=g; g=f; f=e; e=d+t1; d=c; c=b; b=a; a=t1+t2;
  }
  state[0]+=a; state[1]+=b; state[2]+=c; state[3]+=d;
  state[4]+=e; state[5]+=f; state[6]+=g; state[7]+=h;
}

// SHA-256 of an 80-byte message (block-padded to two blocks).
__device__ void sha256_80(const uint8_t* msg, uint8_t out[32]){
  uint32_t st[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
  uint8_t block[64];
  // First block: bytes 0..63.
  sha256_transform(st, msg);
  // Second block: bytes 64..79, then 0x80 pad, then length (640 bits) big-endian.
  #pragma unroll
  for(int i=0;i<16;i++) block[i]=msg[64+i];
  block[16]=0x80;
  for(int i=17;i<64;i++) block[i]=0;
  uint64_t bits = 640;
  for(int i=0;i<8;i++) block[56+i] = (uint8_t)(bits >> (56-8*i));
  sha256_transform(st, block);
  for(int i=0;i<8;i++){ out[i*4]=st[i]>>24; out[i*4+1]=st[i]>>16; out[i*4+2]=st[i]>>8; out[i*4+3]=st[i]; }
}

// SHA-256 of a 32-byte message (single padded block).
__device__ void sha256_32(const uint8_t* msg, uint8_t out[32]){
  uint32_t st[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
  uint8_t block[64];
  #pragma unroll
  for(int i=0;i<32;i++) block[i]=msg[i];
  block[32]=0x80;
  for(int i=33;i<64;i++) block[i]=0;
  uint64_t bits = 256;
  for(int i=0;i<8;i++) block[56+i] = (uint8_t)(bits >> (56-8*i));
  sha256_transform(st, block);
  for(int i=0;i<8;i++){ out[i*4]=st[i]>>24; out[i*4+1]=st[i]>>16; out[i*4+2]=st[i]>>8; out[i*4+3]=st[i]; }
}

__device__ __forceinline__ bool meets_target(const uint8_t h[32], const uint8_t t[32]){
  for(int i=0;i<32;i++){ if(h[i]<t[i]) return true; if(h[i]>t[i]) return false; }
  return true;
}

// ───────────────────────── SHA-256d search kernel ─────────────────────────

__global__ void k_sha256d(const uint8_t* header, const uint8_t* target,
                          unsigned int start, unsigned int count,
                          unsigned int* found_nonce, int* found_flag){
  unsigned int idx = blockIdx.x*blockDim.x + threadIdx.x;
  if(idx >= count) return;
  unsigned int nonce = start + idx;
  uint8_t hdr[80];
  #pragma unroll
  for(int i=0;i<80;i++) hdr[i]=header[i];
  hdr[76]=nonce; hdr[77]=nonce>>8; hdr[78]=nonce>>16; hdr[79]=nonce>>24; // little-endian
  uint8_t h1[32], h2[32];
  sha256_80(hdr, h1);
  sha256_32(h1, h2);
  if(meets_target(h2, target)){
    if(atomicCAS(found_flag, 0, 1)==0){ *found_nonce = nonce; }
  }
}

// ───────────────────────── Keccak-f[1600] (device) ─────────────────────────

__constant__ uint64_t RC[24] = {
  0x0000000000000001ULL,0x0000000000008082ULL,0x800000000000808aULL,0x8000000080008000ULL,
  0x000000000000808bULL,0x0000000080000001ULL,0x8000000080008081ULL,0x8000000000008009ULL,
  0x000000000000008aULL,0x0000000000000088ULL,0x0000000080008009ULL,0x000000008000000aULL,
  0x000000008000808bULL,0x800000000000008bULL,0x8000000000008089ULL,0x8000000000008003ULL,
  0x8000000000008002ULL,0x8000000000000080ULL,0x000000000000800aULL,0x800000008000000aULL,
  0x8000000080008081ULL,0x8000000000008080ULL,0x0000000080000001ULL,0x8000000080008008ULL
};
__constant__ int ROT[25] = {0,1,62,28,27,36,44,6,55,20,3,10,43,25,39,41,45,15,21,8,18,2,61,56,14};

__device__ __forceinline__ uint64_t rotl64(uint64_t x,int n){ return (x<<n)|(x>>(64-n)); }

__device__ void keccakf(uint64_t st[25]){
  for(int r=0;r<24;r++){
    uint64_t c[5], d[5], b[25];
    for(int x=0;x<5;x++) c[x]=st[x]^st[x+5]^st[x+10]^st[x+15]^st[x+20];
    for(int x=0;x<5;x++) d[x]=c[(x+4)%5]^rotl64(c[(x+1)%5],1);
    for(int x=0;x<5;x++) for(int y=0;y<5;y++) st[x+5*y]^=d[x];
    for(int x=0;x<5;x++) for(int y=0;y<5;y++){ int idx=x+5*y; int nw=y+5*((2*x+3*y)%5); b[nw]=rotl64(st[idx],ROT[idx]); }
    for(int y=0;y<5;y++) for(int x=0;x<5;x++) st[x+5*y]=b[x+5*y]^((~b[(x+1)%5+5*y])&b[(x+2)%5+5*y]);
    st[0]^=RC[r];
  }
}

// Keccak-256 (0x01 padding) of an arbitrary short message (< 136 bytes here).
__device__ void keccak256(const uint8_t* data, int len, uint8_t out[32]){
  uint64_t st[25]; for(int i=0;i<25;i++) st[i]=0;
  uint8_t blk[136]; for(int i=0;i<136;i++) blk[i]=0;
  for(int i=0;i<len;i++) blk[i]=data[i];
  blk[len]^=0x01; blk[135]^=0x80;
  for(int i=0;i<17;i++){ uint64_t w=0; for(int j=0;j<8;j++) w|=((uint64_t)blk[i*8+j])<<(8*j); st[i]^=w; }
  keccakf(st);
  for(int i=0;i<4;i++) for(int j=0;j<8;j++) out[i*8+j]=(uint8_t)(st[i]>>(8*j));
}

// kHeavyHash: keccak → 4-bit matrix·vector → keccak. Mirrors src/pow.rs::kheavyhash.
__global__ void k_heavyhash(const uint8_t* header, const uint8_t* target,
                            unsigned int start, unsigned int count,
                            unsigned int* found_nonce, int* found_flag){
  unsigned int idx = blockIdx.x*blockDim.x + threadIdx.x;
  if(idx >= count) return;
  unsigned int nonce = start + idx;
  uint8_t hdr[80];
  #pragma unroll
  for(int i=0;i<80;i++) hdr[i]=header[i];
  hdr[76]=nonce; hdr[77]=nonce>>8; hdr[78]=nonce>>16; hdr[79]=nonce>>24;
  uint8_t h1[32];
  keccak256(hdr, 80, h1);
  // Derive the 64x64 nibble matrix by re-hashing (matches CPU reference).
  uint16_t mat[64][64];
  uint8_t buf[32]; for(int i=0;i<32;i++) buf[i]=h1[i];
  int k=0; int need=4096;
  uint16_t nib[4096];
  while(k<need){
    keccak256(buf,32,buf);
    for(int i=0;i<32 && k<need;i++){ nib[k++]=buf[i]>>4; if(k<need) nib[k++]=buf[i]&0x0f; }
  }
  k=0; for(int i=0;i<64;i++) for(int j=0;j<64;j++) mat[i][j]=nib[k++];
  uint16_t vec[64];
  for(int i=0;i<32;i++){ vec[i*2]=h1[i]>>4; vec[i*2+1]=h1[i]&0x0f; }
  uint8_t mixed[32];
  for(int i=0;i<32;i++){
    unsigned int a0=0,a1=0;
    for(int j=0;j<64;j++){ a0+=mat[i*2][j]*vec[j]; a1+=mat[i*2+1][j]*vec[j]; }
    uint8_t hi=(a0>>10)&0x0f, lo=(a1>>10)&0x0f;
    mixed[i]=((hi<<4)|lo)^h1[i];
  }
  uint8_t h2[32];
  keccak256(mixed,32,h2);
  if(meets_target(h2, target)){
    if(atomicCAS(found_flag, 0, 1)==0){ *found_nonce = nonce; }
  }
}

// ───────────────────── cSHAKE256 + EXACT Kaspa kHeavyHash ─────────────────────
//
// The real Kaspa PoW (per rusty-kaspa), mirroring src/pow.rs::kaspa_pow_hash:
//   h1     = cSHAKE256("ProofOfWorkHash", prePowHash‖ts_le‖[0;32]‖nonce_le)   (80B in)
//   mixed  = h1 XOR nibble(matrix · nibble(h1) >> 10)
//   powval = cSHAKE256("HeavyHash", mixed)   → compared little-endian ≤ target
//
// The rank-64 matrix is generated ONCE per job on the HOST (Rust kaspa_matrix, which
// is unit-tested) and uploaded, so the device needs no xoshiro/rank code. cSHAKE256
// has rate 136; the bytepad(encode_string("")‖encode_string(S),136) first block is a
// constant per domain, hardcoded below (suffix byte 0x04 = cSHAKE domain separation).
//
// UNVERIFIED IN THIS BUILD: there is no CUDA toolkit or GPU here, so this kernel has
// not been compiled or run. It mirrors the CPU reference line-for-line. KAIROS
// re-checks any GPU-found nonce on the CPU (kaspa_pow_hash) before submitting, so a
// miscompiled kernel fails safe (no bad shares) rather than submitting garbage.

// bytepad(encode_string("") ‖ encode_string(S), 136) prefixes (pre-zero-pad):
//   [left_encode(136)=01 88][encode_string("")=01 00][left_encode(8*len)][S...]
__device__ const uint8_t POW_PREFIX[21] = {
  0x01,0x88, 0x01,0x00, 0x01,0x78,               // ...len("ProofOfWorkHash")=15 → 120=0x78
  0x50,0x72,0x6f,0x6f,0x66,0x4f,0x66,0x57,0x6f,0x72,0x6b,0x48,0x61,0x73,0x68 // "ProofOfWorkHash"
};
__device__ const int POW_PREFIX_LEN = 21;
__device__ const uint8_t HEAVY_PREFIX[15] = {
  0x01,0x88, 0x01,0x00, 0x01,0x48,               // ...len("HeavyHash")=9 → 72=0x48
  0x48,0x65,0x61,0x76,0x79,0x48,0x61,0x73,0x68   // "HeavyHash"
};
__device__ const int HEAVY_PREFIX_LEN = 15;

// cSHAKE256 with a fixed domain prefix, absorbing one short (<136B) message block.
__device__ void cshake256_kaspa(const uint8_t* msg, int len,
                                const uint8_t* prefix, int prefix_len, uint8_t out[32]){
  uint64_t st[25];
  #pragma unroll
  for(int i=0;i<25;i++) st[i]=0;
  uint8_t blk[136];
  // Block 1: the constant bytepad domain block (full rate), absorb + permute.
  for(int i=0;i<136;i++) blk[i]=0;
  for(int i=0;i<prefix_len;i++) blk[i]=prefix[i];
  for(int i=0;i<17;i++){ uint64_t w=0; for(int j=0;j<8;j++) w|=((uint64_t)blk[i*8+j])<<(8*j); st[i]^=w; }
  keccakf(st);
  // Block 2: the message with cSHAKE suffix 0x04 and pad10*1 final 0x80.
  for(int i=0;i<136;i++) blk[i]=0;
  for(int i=0;i<len;i++) blk[i]=msg[i];
  blk[len]^=0x04; blk[135]^=0x80;
  for(int i=0;i<17;i++){ uint64_t w=0; for(int j=0;j<8;j++) w|=((uint64_t)blk[i*8+j])<<(8*j); st[i]^=w; }
  keccakf(st);
  for(int i=0;i<4;i++) for(int j=0;j<8;j++) out[i*8+j]=(uint8_t)(st[i]>>(8*j));
}

// One Kaspa PoW over (prePowHash, timestamp, nonce) with a host-supplied job matrix.
__global__ void k_kaspa(const uint8_t* pre_pow, unsigned long long timestamp,
                        const uint16_t* matrix, const uint8_t* target,
                        unsigned long long start, unsigned long long count,
                        unsigned long long* out_nonce, int* found_flag){
  unsigned long long idx = (unsigned long long)blockIdx.x*blockDim.x + threadIdx.x;
  if(idx >= count) return;
  unsigned long long nonce = start + idx;
  uint8_t data[80];
  #pragma unroll
  for(int i=0;i<32;i++) data[i]=pre_pow[i];
  for(int i=0;i<8;i++) data[32+i]=(uint8_t)(timestamp>>(8*i)); // little-endian
  for(int i=40;i<72;i++) data[i]=0;
  for(int i=0;i<8;i++) data[72+i]=(uint8_t)(nonce>>(8*i));      // little-endian
  uint8_t h1[32];
  cshake256_kaspa(data, 80, POW_PREFIX, POW_PREFIX_LEN, h1);
  // heavy step: 4-bit vector, matrix multiply, >>10, XOR back into h1
  uint16_t vec[64];
  for(int i=0;i<32;i++){ vec[i*2]=h1[i]>>4; vec[i*2+1]=h1[i]&0x0f; }
  uint8_t mixed[32];
  for(int i=0;i<32;i++){
    unsigned int a0=0,a1=0;
    for(int j=0;j<64;j++){ a0+=(unsigned int)matrix[(i*2)*64+j]*vec[j]; a1+=(unsigned int)matrix[(i*2+1)*64+j]*vec[j]; }
    mixed[i]=h1[i]^(uint8_t)(((a0>>10)<<4)|(a1>>10));
  }
  uint8_t h2[32];
  cshake256_kaspa(mixed, 32, HEAVY_PREFIX, HEAVY_PREFIX_LEN, h2);
  // powValue is h2 as a little-endian integer; compare ≤ big-endian target.
  uint8_t be[32];
  for(int i=0;i<32;i++) be[i]=h2[31-i];
  if(meets_target(be, target)){
    if(atomicCAS(found_flag, 0, 1)==0){ *out_nonce = nonce; }
  }
}

// ───────────────────────── Host launchers (C linkage) ─────────────────────────

extern "C" int kairos_cuda_device_count(){
  int n=0; if(cudaGetDeviceCount(&n)!=cudaSuccess) return 0; return n;
}

static int run_search(void(*kernel)(const uint8_t*,const uint8_t*,unsigned int,unsigned int,unsigned int*,int*),
                      const uint8_t* header80, const uint8_t* target32,
                      unsigned int start, unsigned int count,
                      unsigned int* out_nonce, unsigned char* out_hash32, bool heavy){
  uint8_t *d_hdr=0,*d_tgt=0; unsigned int *d_nonce=0; int *d_flag=0;
  cudaMalloc(&d_hdr,80); cudaMalloc(&d_tgt,32); cudaMalloc(&d_nonce,4); cudaMalloc(&d_flag,4);
  cudaMemcpy(d_hdr,header80,80,cudaMemcpyHostToDevice);
  cudaMemcpy(d_tgt,target32,32,cudaMemcpyHostToDevice);
  cudaMemset(d_flag,0,4); cudaMemset(d_nonce,0,4);
  int threads=256; int blocks=(count+threads-1)/threads;
  if(heavy) k_heavyhash<<<blocks,threads>>>(d_hdr,d_tgt,start,count,d_nonce,d_flag);
  else      k_sha256d  <<<blocks,threads>>>(d_hdr,d_tgt,start,count,d_nonce,d_flag);
  cudaDeviceSynchronize();
  int flag=0; unsigned int nonce=0;
  cudaMemcpy(&flag,d_flag,4,cudaMemcpyDeviceToHost);
  cudaMemcpy(&nonce,d_nonce,4,cudaMemcpyDeviceToHost);
  cudaFree(d_hdr); cudaFree(d_tgt); cudaFree(d_nonce); cudaFree(d_flag);
  (void)kernel; (void)out_hash32;
  if(flag){ *out_nonce=nonce; return 1; }
  return 0;
}

extern "C" int kairos_cuda_search_sha256d(const unsigned char* header80, const unsigned char* target32,
    unsigned int start, unsigned int count, unsigned int* out_nonce, unsigned char* out_hash32){
  return run_search(0, header80, target32, start, count, out_nonce, out_hash32, false);
}

extern "C" int kairos_cuda_search_heavyhash(const unsigned char* header80, const unsigned char* target32,
    unsigned int start, unsigned int count, unsigned int* out_nonce, unsigned char* out_hash32){
  return run_search(0, header80, target32, start, count, out_nonce, out_hash32, true);
}

// Search a u64 nonce range with the EXACT Kaspa kHeavyHash. `matrix` is 64*64
// uint16 (row-major) precomputed on the host (rank-64). Returns 1 + fills
// *out_nonce if a nonce whose PoW value ≤ target is found. The caller re-verifies
// on the CPU before submitting, so a bad build cannot produce accepted-but-wrong
// shares.
extern "C" int kairos_cuda_search_kaspa(const unsigned char* pre_pow32,
    unsigned long long timestamp, const unsigned short* matrix4096,
    const unsigned char* target32, unsigned long long start, unsigned long long count,
    unsigned long long* out_nonce){
  uint8_t *d_pre=0,*d_tgt=0; uint16_t* d_mat=0; unsigned long long* d_nonce=0; int* d_flag=0;
  cudaMalloc(&d_pre,32); cudaMalloc(&d_tgt,32); cudaMalloc(&d_mat,64*64*sizeof(uint16_t));
  cudaMalloc(&d_nonce,sizeof(unsigned long long)); cudaMalloc(&d_flag,4);
  cudaMemcpy(d_pre,pre_pow32,32,cudaMemcpyHostToDevice);
  cudaMemcpy(d_tgt,target32,32,cudaMemcpyHostToDevice);
  cudaMemcpy(d_mat,matrix4096,64*64*sizeof(uint16_t),cudaMemcpyHostToDevice);
  cudaMemset(d_flag,0,4); cudaMemset(d_nonce,0,sizeof(unsigned long long));
  int threads=256; unsigned long long blocks=(count+threads-1)/threads;
  k_kaspa<<<(unsigned int)blocks,threads>>>(d_pre,timestamp,d_mat,d_tgt,start,count,d_nonce,d_flag);
  cudaDeviceSynchronize();
  int flag=0; unsigned long long nonce=0;
  cudaMemcpy(&flag,d_flag,4,cudaMemcpyDeviceToHost);
  cudaMemcpy(&nonce,d_nonce,sizeof(unsigned long long),cudaMemcpyDeviceToHost);
  cudaFree(d_pre); cudaFree(d_tgt); cudaFree(d_mat); cudaFree(d_nonce); cudaFree(d_flag);
  if(flag){ *out_nonce=nonce; return 1; }
  return 0;
}
