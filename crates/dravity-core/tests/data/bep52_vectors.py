# Independent BEP 52 reference used to generate VECTORS in
# ../bep52_conformance.rs: python3 bep52_vectors.py > vectors.rs
import hashlib
L=16384
def H(b): return hashlib.sha256(b).digest()
def np2(n):
    p=1
    while p<n: p*=2
    return p
def tree_root(nodes, width, padnode):
    layer=nodes+[padnode]*(width-len(nodes))
    pad=padnode
    while len(layer)>1:
        layer=[H(layer[i]+layer[i+1]) for i in range(0,len(layer),2)]
    return layer[0]
def file_vec(data, plen):
    leaves=[H(data[i:i+L]) for i in range(0,len(data),L)]
    per=plen//L
    if len(data)<=plen:
        return tree_root(leaves,np2(len(leaves)),b'\0'*32),[]
    width=np2(len(leaves))
    # full tree root
    root=tree_root(leaves,width,b'\0'*32)
    layer=[]
    for i in range(0,len(leaves),per):
        chunk=leaves[i:i+per]
        layer.append(tree_root(chunk,per,b'\0'*32))
    return root,layer
def data(n): return bytes((i*7+3)%251 for i in range(n))
out=[]
for plen in (16384,32768,65536):
    for n in (1,100,16383,16384,16385,32767,32768,32769,65536,65537,3*16384+5,7*16384,9*16384+1):
        r,layer=file_vec(data(n),plen)
        out.append((plen,n,r.hex(),[h.hex() for h in layer]))
print("const VECTORS: &[(u32, usize, &str, &[&str])] = &[")
for plen,n,r,layer in out:
    print(f'    ({plen}, {n}, "{r}", &[' + ", ".join(f'"{h}"' for h in layer) + "]),")
print("];")
