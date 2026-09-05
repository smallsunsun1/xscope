package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"xscope.dev/xscope/control-plane/internal/platform"
)

func TestHealth(t *testing.T) {
	app := &server{store: platform.NewStore()}
	recorder := httptest.NewRecorder()
	app.routes().ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, "/healthz", nil))
	if recorder.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", recorder.Code)
	}
	if recorder.Header().Get("X-Request-Id") == "" {
		t.Fatal("missing request id")
	}
}

func TestCreateProjectAndIssueAPIKey(t *testing.T) {
	app := &server{store: platform.NewStore()}

	projectRequest := httptest.NewRequest(
		http.MethodPost,
		"/admin/v1/projects",
		strings.NewReader(`{"id":"p1","tenant_id":"t1","name":"demo"}`),
	)
	projectRequest.Header.Set("Idempotency-Key", "create-project-key")
	projectRecorder := httptest.NewRecorder()
	app.routes().ServeHTTP(projectRecorder, projectRequest)
	if projectRecorder.Code != http.StatusCreated {
		t.Fatalf("project status = %d, body = %s", projectRecorder.Code, projectRecorder.Body.String())
	}

	keyRequest := httptest.NewRequest(
		http.MethodPost,
		"/admin/v1/api-keys",
		strings.NewReader(`{"id":"key-1","tenant_id":"t1","project_id":"p1","name":"local"}`),
	)
	keyRequest.Header.Set("Idempotency-Key", "create-api-key-1")
	keyRecorder := httptest.NewRecorder()
	app.routes().ServeHTTP(keyRecorder, keyRequest)
	if keyRecorder.Code != http.StatusCreated {
		t.Fatalf("key status = %d, body = %s", keyRecorder.Code, keyRecorder.Body.String())
	}
	var issued platform.IssuedAPIKey
	if err := json.NewDecoder(keyRecorder.Body).Decode(&issued); err != nil {
		t.Fatal(err)
	}
	if issued.Secret == "" {
		t.Fatal("missing one-time API key secret")
	}
	if metadata, ok := app.store.VerifyAPIKey(issued.Secret); !ok || metadata.ProjectID != "p1" {
		t.Fatal("issued API key was not stored as a verifiable digest")
	}
}

func TestCreateProjectRequiresIdempotencyKey(t *testing.T) {
	app := &server{store: platform.NewStore()}
	recorder := httptest.NewRecorder()
	request := httptest.NewRequest(
		http.MethodPost,
		"/admin/v1/projects",
		strings.NewReader(`{"id":"p1","tenant_id":"t1","name":"demo"}`),
	)
	app.routes().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", recorder.Code)
	}
}
