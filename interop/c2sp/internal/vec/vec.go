// Package vec holds what the generator and the checker share: the vector
// file format, deterministic cosigners and an in-process stock witness.
//
// A vector file is UTF-8 text:
//
//	# <one comment line naming the vector>
//	# input
//	key: value
//	# expect
//	key: value
//
// Values are one line. Byte strings are standard base64 under a key ending
// in "_b64". A key may repeat; order is kept.
package vec

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"filippo.io/mldsa"
	"github.com/transparency-dev/witness/persistence/inmemory"
	"github.com/transparency-dev/witness/witness"
	"golang.org/x/mod/sumdb/note"
	"k8s.io/klog/v2"
)

func init() {
	// The stock witness logs refused checkpoints; the vectors record them.
	klog.SetOutput(io.Discard)
	klog.LogToStderr(false)
}

// Field is one key and its value.
type Field struct{ Key, Value string }

// Vector is one file.
type Vector struct {
	Comment string
	Input   []Field
	Expect  []Field
}

// In adds an input field.
func (v *Vector) In(key string, value any) *Vector {
	v.Input = append(v.Input, field(key, value))
	return v
}

// Out adds an expected field.
func (v *Vector) Out(key string, value any) *Vector {
	v.Expect = append(v.Expect, field(key, value))
	return v
}

func field(key string, value any) Field {
	switch value := value.(type) {
	case []byte:
		if !strings.HasSuffix(key, "_b64") {
			panic("byte values need a _b64 key: " + key)
		}
		return Field{key, base64.StdEncoding.EncodeToString(value)}
	case string:
		if strings.ContainsAny(value, "\n\r") {
			panic("multi-line value for " + key)
		}
		return Field{key, value}
	default:
		return Field{key, fmt.Sprint(value)}
	}
}

// Bytes renders the file.
func (v *Vector) Bytes() []byte {
	var b bytes.Buffer
	fmt.Fprintf(&b, "# %s\n# input\n", v.Comment)
	for _, f := range v.Input {
		fmt.Fprintf(&b, "%s: %s\n", f.Key, f.Value)
	}
	b.WriteString("# expect\n")
	for _, f := range v.Expect {
		fmt.Fprintf(&b, "%s: %s\n", f.Key, f.Value)
	}
	return b.Bytes()
}

// Parse reads a vector file.
func Parse(data []byte) (*Vector, error) {
	v := &Vector{}
	section := ""
	for i, line := range strings.Split(strings.TrimSuffix(string(data), "\n"), "\n") {
		switch {
		case i == 0 && strings.HasPrefix(line, "# "):
			v.Comment = strings.TrimPrefix(line, "# ")
		case line == "# input" || line == "# expect":
			section = line
		default:
			key, value, ok := strings.Cut(line, ": ")
			if !ok {
				return nil, fmt.Errorf("line %d: %q", i+1, line)
			}
			if section == "# input" {
				v.Input = append(v.Input, Field{key, value})
			} else {
				v.Expect = append(v.Expect, Field{key, value})
			}
		}
	}
	return v, nil
}

// Get returns the first value of key in the fields.
func Get(fields []Field, key string) string {
	for _, f := range fields {
		if f.Key == key {
			return f.Value
		}
	}
	panic("no field " + key)
}

// All returns every value of key.
func All(fields []Field, key string) []string {
	var out []string
	for _, f := range fields {
		if f.Key == key {
			out = append(out, f.Value)
		}
	}
	return out
}

// B64 decodes a base64 field.
func B64(fields []Field, key string) []byte {
	b, err := base64.StdEncoding.DecodeString(Get(fields, key))
	if err != nil {
		panic(err)
	}
	return b
}

// Writer writes vectors below a root and records their SHA-256s.
type Writer struct {
	Root  string
	files map[string][32]byte
}

// NewWriter empties root and returns a writer into it.
func NewWriter(root string) *Writer {
	if err := os.RemoveAll(root); err != nil {
		panic(err)
	}
	return &Writer{Root: root, files: map[string][32]byte{}}
}

// Write writes one vector at a path relative to the root.
func (w *Writer) Write(path string, v *Vector) {
	w.Raw(path, v.Bytes())
}

// Raw writes one file at a path relative to the root.
func (w *Writer) Raw(path string, data []byte) {
	full := filepath.Join(w.Root, filepath.FromSlash(path))
	if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
		panic(err)
	}
	if err := os.WriteFile(full, data, 0o644); err != nil {
		panic(err)
	}
	if _, dup := w.files[path]; dup {
		panic("written twice: " + path)
	}
	w.files[path] = sha256.Sum256(data)
}

// Manifest writes MANIFEST: one "<sha256 hex>  <path>" line per file,
// sorted by path.
func (w *Writer) Manifest() {
	paths := make([]string, 0, len(w.files))
	for p := range w.files {
		paths = append(paths, p)
	}
	sort.Strings(paths)
	var b bytes.Buffer
	for _, p := range paths {
		sum := w.files[p]
		fmt.Fprintf(&b, "%x  %s\n", sum, p)
	}
	if err := os.WriteFile(filepath.Join(w.Root, "MANIFEST"), b.Bytes(), 0o644); err != nil {
		panic(err)
	}
}

// KeyHash is the C2SP key ID: SHA-256(name || 0x0A || type || key)[:4].
func KeyHash(name string, typeAndKey []byte) uint32 {
	h := sha256.New()
	h.Write([]byte(name))
	h.Write([]byte{'\n'})
	h.Write(typeAndKey)
	return binary.BigEndian.Uint32(h.Sum(nil))
}

// VKey renders a vkey.
func VKey(name string, typeAndKey []byte) string {
	return fmt.Sprintf("%s+%08x+%s", name, KeyHash(name, typeAndKey), base64.StdEncoding.EncodeToString(typeAndKey))
}

// Ed25519 is an Ed25519 key from a 32-byte seed.
func Ed25519(seed byte) ed25519.PrivateKey {
	return ed25519.NewKeyFromSeed(bytes.Repeat([]byte{seed}, 32))
}

// LogSigner is a note.Signer for a 0x01 Ed25519 log key.
type LogSigner struct {
	KeyName string
	Key     ed25519.PrivateKey
}

func (s LogSigner) Name() string { return s.KeyName }
func (s LogSigner) KeyHash() uint32 {
	return KeyHash(s.KeyName, s.typed())
}
func (s LogSigner) Sign(msg []byte) ([]byte, error) { return ed25519.Sign(s.Key, msg), nil }
func (s LogSigner) typed() []byte {
	return append([]byte{0x01}, s.Key.Public().(ed25519.PublicKey)...)
}

// VKey is the log key's vkey.
func (s LogSigner) VKey() string { return VKey(s.KeyName, s.typed()) }

// SKey is the log key's Go signer key text (for note.NewSigner).
func (s LogSigner) SKey() string {
	return fmt.Sprintf("PRIVATE+KEY+%s+%08x+%s", s.KeyName, s.KeyHash(),
		base64.StdEncoding.EncodeToString(append([]byte{0x01}, s.Key.Seed()...)))
}

// Cosigner is a note.Signer for a 0x04 Ed25519 cosignature/v1 key with a
// fixed timestamp, so every vector is reproducible. The message is the one
// c2sp.org/tlog-cosignature specifies; every cosignature it makes is
// checked with formats/note.NewVerifierForCosignatureV1 by the callers.
type Cosigner struct {
	KeyName string
	Key     ed25519.PrivateKey
	Time    uint64
}

func (s *Cosigner) Name() string { return s.KeyName }
func (s *Cosigner) KeyHash() uint32 {
	return KeyHash(s.KeyName, s.typed())
}
func (s *Cosigner) Sign(msg []byte) ([]byte, error) {
	m := fmt.Appendf(nil, "cosignature/v1\ntime %d\n%s", s.Time, msg)
	sig := binary.BigEndian.AppendUint64(nil, s.Time)
	return append(sig, ed25519.Sign(s.Key, m)...), nil
}
func (s *Cosigner) typed() []byte {
	return append([]byte{0x04}, s.Key.Public().(ed25519.PublicKey)...)
}

// VKey is the cosigner's vkey.
func (s *Cosigner) VKey() string { return VKey(s.KeyName, s.typed()) }

// MLDSACosigner is a note.Signer for a 0x06 ML-DSA-44 checkpoint
// cosignature with a fixed timestamp and deterministic signing.
type MLDSACosigner struct {
	KeyName string
	Key     *mldsa.PrivateKey
	Time    uint64
}

// NewMLDSACosigner derives the key from a 32-byte seed.
func NewMLDSACosigner(name string, seed byte, time uint64) *MLDSACosigner {
	key, err := mldsa.NewPrivateKey(mldsa.MLDSA44(), bytes.Repeat([]byte{seed}, 32))
	if err != nil {
		panic(err)
	}
	return &MLDSACosigner{KeyName: name, Key: key, Time: time}
}

func (s *MLDSACosigner) Name() string { return s.KeyName }
func (s *MLDSACosigner) KeyHash() uint32 {
	return KeyHash(s.KeyName, s.typed())
}
func (s *MLDSACosigner) typed() []byte {
	return append([]byte{0x06}, s.Key.PublicKey().Bytes()...)
}

// VKey is the cosigner's vkey.
func (s *MLDSACosigner) VKey() string { return VKey(s.KeyName, s.typed()) }

// Sign signs the CosignedSubtree structure of c2sp.org/tlog-cosignature
// for the checkpoint in msg (start 0, end = size).
func (s *MLDSACosigner) Sign(msg []byte) ([]byte, error) {
	lines := strings.SplitN(string(msg), "\n", 4)
	var size uint64
	if _, err := fmt.Sscanf(lines[1], "%d", &size); err != nil {
		return nil, err
	}
	root, err := base64.StdEncoding.DecodeString(lines[2])
	if err != nil {
		return nil, err
	}
	m := []byte("subtree/v1\n\x00")
	m = append(m, byte(len(s.KeyName)))
	m = append(m, s.KeyName...)
	m = binary.BigEndian.AppendUint64(m, s.Time)
	m = append(m, byte(len(lines[0])))
	m = append(m, lines[0]...)
	m = binary.BigEndian.AppendUint64(m, 0)
	m = binary.BigEndian.AppendUint64(m, size)
	m = append(m, root...)
	sig, err := s.Key.SignDeterministic(m, nil)
	if err != nil {
		return nil, err
	}
	return append(binary.BigEndian.AppendUint64(nil, s.Time), sig...), nil
}

// Witness is the stock transparency-dev witness, in process, behind its
// own HTTP handler.
type Witness struct {
	Server *httptest.Server
}

// NewWitness starts a stock witness that signs with signer and trusts the
// given log vkeys (origin = key name).
func NewWitness(signer note.Signer, logs ...string) *Witness {
	verifiers := map[string]note.Verifier{}
	for _, vkey := range logs {
		v, err := note.NewVerifier(vkey)
		if err != nil {
			panic(err)
		}
		verifiers[v.Name()] = v
	}
	w, err := witness.New(context.Background(), witness.Opts{
		Persistence: inmemory.New(),
		Signers:     []note.Signer{signer},
		VerifierForLog: func(_ context.Context, origin string) (note.Verifier, bool, error) {
			v, ok := verifiers[origin]
			return v, ok, nil
		},
	})
	if err != nil {
		panic(err)
	}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /add-checkpoint", witness.NewHTTPHandler(w).AddCheckpoint)
	return &Witness{Server: httptest.NewServer(mux)}
}

// Reply is one witness answer.
type Reply struct {
	Status      int
	ContentType string
	Body        []byte
}

// Post sends one add-checkpoint body.
func (w *Witness) Post(body []byte) Reply {
	resp, err := http.Post(w.Server.URL+"/add-checkpoint", "text/plain", bytes.NewReader(body))
	if err != nil {
		panic(err)
	}
	defer resp.Body.Close()
	var b bytes.Buffer
	if _, err := b.ReadFrom(resp.Body); err != nil {
		panic(err)
	}
	return Reply{resp.StatusCode, resp.Header.Get("Content-Type"), b.Bytes()}
}

// Close stops the server.
func (w *Witness) Close() { w.Server.Close() }
