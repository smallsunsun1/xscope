package platform

import (
	"bytes"
	"errors"
	"testing"
	"time"
)

func TestQuoteRoundsUpAndPinsPrice(t *testing.T) {
	store := NewStore()
	quote, err := store.Quote("xscope-demo", 1, 1)
	if err != nil {
		t.Fatal(err)
	}
	if quote.Maximum.Amount != 2 {
		t.Fatalf("amount = %d, want 2", quote.Maximum.Amount)
	}
	if quote.PriceVersion == "" {
		t.Fatal("quote must pin a price version")
	}
}

func TestPutProjectValidatesTenantBoundary(t *testing.T) {
	store := NewStore()
	if err := store.PutProject(Project{ID: "p1", Name: "missing tenant"}); err == nil {
		t.Fatal("expected missing tenant to fail")
	}
}

func TestCreateProjectIsIdempotent(t *testing.T) {
	store := NewStore()
	project := Project{ID: "p1", TenantID: "t1", Name: "demo"}
	if _, created, err := store.CreateProject("idempotency-key-1", project); err != nil || !created {
		t.Fatalf("first create = created %v, err %v", created, err)
	}
	if _, created, err := store.CreateProject("idempotency-key-1", project); err != nil || created {
		t.Fatalf("replay = created %v, err %v", created, err)
	}
}

func TestIssueAPIKeyStoresHashAndPreservesTenantBoundary(t *testing.T) {
	store := NewStore()
	if err := store.PutProject(Project{ID: "p1", TenantID: "t1", Name: "demo"}); err != nil {
		t.Fatal(err)
	}
	request := APIKeyRequest{ID: "key-1", TenantID: "t1", ProjectID: "p1", Name: "local"}
	issued, created, err := store.issueAPIKey(
		"idempotency-key-1",
		request,
		bytes.NewReader(make([]byte, 24)),
		time.Date(2026, 9, 4, 0, 0, 0, 0, time.UTC),
	)
	if err != nil || !created {
		t.Fatalf("issue = created %v, err %v", created, err)
	}
	if issued.Secret == "" {
		t.Fatal("issued key must return its secret once")
	}
	metadata, ok := store.VerifyAPIKey(issued.Secret)
	if !ok || metadata.ID != "key-1" {
		t.Fatalf("issued secret did not verify: %#v, %v", metadata, ok)
	}
	if _, ok := store.VerifyAPIKey("wrong"); ok {
		t.Fatal("unknown secret verified")
	}

	replay, replayCreated, err := store.issueAPIKey(
		"idempotency-key-1",
		request,
		bytes.NewReader(bytes.Repeat([]byte{1}, 24)),
		time.Now(),
	)
	if err != nil || replayCreated || replay.Secret != issued.Secret {
		t.Fatalf("idempotent replay = %#v, created %v, err %v", replay, replayCreated, err)
	}

	badRequest := APIKeyRequest{ID: "key-2", TenantID: "other", ProjectID: "p1", Name: "bad"}
	if _, _, err := store.issueAPIKey("idempotency-key-2", badRequest, bytes.NewReader(make([]byte, 24)), time.Now()); err == nil {
		t.Fatal("cross-tenant API key must fail")
	}
}

func TestListAndRevokeAPIKeys(t *testing.T) {
	store := NewStore()
	if err := store.PutProject(Project{ID: "project-1", TenantID: "tenant-1", Name: "Demo"}); err != nil {
		t.Fatal(err)
	}
	issued, _, err := store.IssueAPIKey("issue-key-000001", APIKeyRequest{
		ID: "key-1", TenantID: "tenant-1", ProjectID: "project-1", Name: "Local",
	})
	if err != nil {
		t.Fatal(err)
	}
	if got := store.ListProjects(); len(got) != 1 || got[0].ID != "project-1" {
		t.Fatalf("projects = %#v", got)
	}
	if got := store.ListAPIKeys(); len(got) != 1 || got[0].ID != "key-1" {
		t.Fatalf("api keys = %#v", got)
	}
	if err := store.RevokeAPIKey("key-1"); err != nil {
		t.Fatal(err)
	}
	if _, ok := store.VerifyAPIKey(issued.Secret); ok {
		t.Fatal("revoked API key is still valid")
	}
	if err := store.RevokeAPIKey("key-1"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("revoke error = %v, want ErrNotFound", err)
	}
}
