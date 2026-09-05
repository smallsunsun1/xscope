package main

import (
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"os"
	"strconv"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/google/uuid"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
	"xscope.dev/xscope/control-plane/internal/cluster"
	"xscope.dev/xscope/control-plane/internal/platform"
)

type server struct {
	store       *platform.Store
	deployments *cluster.Service
}

type scaleRequest struct {
	Replicas int32 `json:"replicas"`
}

func main() {
	address := envOr("XSCOPE_CONTROL_ADDRESS", ":8081")
	deploymentService, err := cluster.New()
	if err != nil {
		slog.Warn("Kubernetes deployment management unavailable", "error", err)
		deploymentService = cluster.Unavailable(err)
	}
	app := &server{store: platform.NewStore(), deployments: deploymentService}
	httpServer := &http.Server{
		Addr:              address,
		Handler:           app.routes(),
		ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout:       60 * time.Second,
	}

	slog.Info("control plane listening", "address", address)
	if err := httpServer.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		slog.Error("control plane stopped", "error", err)
		os.Exit(1)
	}
}

func (s *server) routes() http.Handler {
	router := chi.NewRouter()
	router.Use(middleware.Recoverer)
	router.Use(middleware.Timeout(30 * time.Second))
	router.Use(requestContext)
	router.Get("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok", "component": "control-plane"})
	})
	router.Get("/readyz", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ready"})
	})
	router.Get("/v1/models", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": s.store.ListModels()})
	})
	router.Route("/admin/v1", func(router chi.Router) {
		router.Get("/projects", s.listProjects)
		router.Post("/projects", s.createProject)
		router.Get("/api-keys", s.listAPIKeys)
		router.Post("/api-keys", s.createAPIKey)
		router.Delete("/api-keys/{id}", s.revokeAPIKey)
		router.Get("/quote", s.quote)
		router.Get("/model-deployments", s.listModelDeployments)
		router.Post("/model-deployments", s.createModelDeployment)
		router.Put("/model-deployments/{namespace}/{name}/scale", s.scaleModelDeployment)
		router.Delete("/model-deployments/{namespace}/{name}", s.deleteModelDeployment)
	})
	return router
}

func (s *server) listProjects(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": s.store.ListProjects()})
}

func (s *server) createProject(w http.ResponseWriter, r *http.Request) {
	defer r.Body.Close()
	var project platform.Project
	if err := decodeJSON(w, r, &project); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "invalid JSON body")
		return
	}
	idempotencyKey, ok := requireIdempotencyKey(w, r)
	if !ok {
		return
	}
	createdProject, created, err := s.store.CreateProject(idempotencyKey, project)
	if err != nil {
		writePlatformError(w, err)
		return
	}
	status := http.StatusOK
	if created {
		status = http.StatusCreated
	}
	writeJSON(w, status, createdProject)
}

func (s *server) createAPIKey(w http.ResponseWriter, r *http.Request) {
	defer r.Body.Close()
	var request platform.APIKeyRequest
	if err := decodeJSON(w, r, &request); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "invalid JSON body")
		return
	}
	idempotencyKey, ok := requireIdempotencyKey(w, r)
	if !ok {
		return
	}
	issued, created, err := s.store.IssueAPIKey(idempotencyKey, request)
	if err != nil {
		writePlatformError(w, err)
		return
	}
	status := http.StatusOK
	if created {
		status = http.StatusCreated
	}
	writeJSON(w, status, issued)
}

func (s *server) listAPIKeys(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": s.store.ListAPIKeys()})
}

func (s *server) revokeAPIKey(w http.ResponseWriter, r *http.Request) {
	if err := s.store.RevokeAPIKey(chi.URLParam(r, "id")); err != nil {
		if errors.Is(err, platform.ErrNotFound) {
			writeError(w, http.StatusNotFound, "not_found", err.Error())
			return
		}
		writeError(w, http.StatusInternalServerError, "internal_error", "could not revoke API key")
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *server) quote(w http.ResponseWriter, r *http.Request) {
	input, inputErr := strconv.ParseInt(r.URL.Query().Get("input_tokens"), 10, 64)
	output, outputErr := strconv.ParseInt(r.URL.Query().Get("output_tokens"), 10, 64)
	if inputErr != nil || outputErr != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "input_tokens and output_tokens must be integers")
		return
	}
	quote, err := s.store.Quote(r.URL.Query().Get("model"), input, output)
	if err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", err.Error())
		return
	}
	writeJSON(w, http.StatusOK, quote)
}

func (s *server) listModelDeployments(w http.ResponseWriter, r *http.Request) {
	if !s.clusterAvailable(w) {
		return
	}
	namespace := r.URL.Query().Get("namespace")
	if namespace == "" {
		namespace = "xscope-system"
	}
	deployments, err := s.deployments.List(r.Context(), namespace)
	if err != nil {
		writeClusterError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": deployments})
}

func (s *server) createModelDeployment(w http.ResponseWriter, r *http.Request) {
	if !s.clusterAvailable(w) {
		return
	}
	defer r.Body.Close()
	var deployment platformv1alpha1.ModelDeployment
	if err := decodeJSON(w, r, &deployment); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "invalid JSON body")
		return
	}
	if deployment.Namespace == "" {
		deployment.Namespace = "xscope-system"
	}
	if err := s.deployments.Create(r.Context(), &deployment); err != nil {
		writeClusterError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, deployment)
}

func (s *server) scaleModelDeployment(w http.ResponseWriter, r *http.Request) {
	if !s.clusterAvailable(w) {
		return
	}
	defer r.Body.Close()
	var request scaleRequest
	if err := decodeJSON(w, r, &request); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "invalid JSON body")
		return
	}
	deployment, err := s.deployments.Scale(
		r.Context(),
		chi.URLParam(r, "namespace"),
		chi.URLParam(r, "name"),
		request.Replicas,
	)
	if err != nil {
		writeClusterError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, deployment)
}

func (s *server) deleteModelDeployment(w http.ResponseWriter, r *http.Request) {
	if !s.clusterAvailable(w) {
		return
	}
	if err := s.deployments.Delete(r.Context(), chi.URLParam(r, "namespace"), chi.URLParam(r, "name")); err != nil {
		writeClusterError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *server) clusterAvailable(w http.ResponseWriter) bool {
	if s.deployments != nil && s.deployments.Available() {
		return true
	}
	writeError(w, http.StatusServiceUnavailable, "cluster_unavailable", cluster.ErrUnavailable.Error())
	return false
}

func writeClusterError(w http.ResponseWriter, err error) {
	switch {
	case errors.Is(err, cluster.ErrUnavailable):
		writeError(w, http.StatusServiceUnavailable, "cluster_unavailable", err.Error())
	case apierrors.IsNotFound(err):
		writeError(w, http.StatusNotFound, "not_found", "model deployment not found")
	case apierrors.IsAlreadyExists(err):
		writeError(w, http.StatusConflict, "conflict", "model deployment already exists")
	default:
		writeError(w, http.StatusBadRequest, "invalid_request", err.Error())
	}
}

func requestContext(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requestID := r.Header.Get("X-Request-Id")
		if requestID == "" || len(requestID) > 128 {
			requestID = uuid.NewString()
		}
		w.Header().Set("X-Request-Id", requestID)
		next.ServeHTTP(w, r)
	})
}

func decodeJSON(w http.ResponseWriter, r *http.Request, target any) error {
	decoder := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(target); err != nil {
		return err
	}
	if err := decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		return errors.New("request body must contain one JSON object")
	}
	return nil
}

func requireIdempotencyKey(w http.ResponseWriter, r *http.Request) (string, bool) {
	key := r.Header.Get("Idempotency-Key")
	if len(key) < 16 || len(key) > 128 {
		writeError(w, http.StatusBadRequest, "invalid_idempotency_key", "Idempotency-Key must be 16 to 128 bytes")
		return "", false
	}
	return key, true
}

func writePlatformError(w http.ResponseWriter, err error) {
	if errors.Is(err, platform.ErrAlreadyExists) || errors.Is(err, platform.ErrIdempotencyConflict) {
		writeError(w, http.StatusConflict, "conflict", err.Error())
		return
	}
	writeError(w, http.StatusBadRequest, "invalid_request", err.Error())
}

func writeError(w http.ResponseWriter, status int, code, message string) {
	writeJSON(w, status, map[string]any{"error": map[string]string{"code": code, "message": message}})
}

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}

func envOr(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}
