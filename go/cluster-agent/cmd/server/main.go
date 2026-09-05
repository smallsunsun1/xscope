package main

import (
	"crypto/sha256"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"os"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
	"xscope.dev/xscope/kubernetes/cluster"
)

type server struct {
	deployments   *cluster.Service
	internalToken string
}

type scaleRequest struct {
	Replicas int32 `json:"replicas"`
}

func main() {
	service, err := cluster.New()
	if err != nil {
		slog.Error("Kubernetes client unavailable", "error", err)
		os.Exit(1)
	}
	app := &server{deployments: service, internalToken: os.Getenv("XSCOPE_INTERNAL_TOKEN")}
	httpServer := &http.Server{
		Addr:              envOr("XSCOPE_CLUSTER_AGENT_ADDRESS", ":8083"),
		Handler:           app.routes(),
		ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	slog.Info("cluster agent listening", "address", httpServer.Addr)
	if err := httpServer.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		slog.Error("cluster agent stopped", "error", err)
		os.Exit(1)
	}
}

func (s *server) routes() http.Handler {
	router := chi.NewRouter()
	router.Use(middleware.Recoverer)
	router.Use(middleware.Timeout(30 * time.Second))
	router.Get("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok", "component": "cluster-agent"})
	})
	router.Get("/readyz", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ready"})
	})
	router.Route("/v1", func(router chi.Router) {
		router.Use(s.requireInternalToken)
		router.Get("/model-deployments", s.listModelDeployments)
		router.Post("/model-deployments", s.createModelDeployment)
		router.Put("/model-deployments/{namespace}/{name}/scale", s.scaleModelDeployment)
		router.Delete("/model-deployments/{namespace}/{name}", s.deleteModelDeployment)
	})
	return router
}

func (s *server) listModelDeployments(w http.ResponseWriter, r *http.Request) {
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
	if err := s.deployments.Delete(r.Context(), chi.URLParam(r, "namespace"), chi.URLParam(r, "name")); err != nil {
		writeClusterError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *server) requireInternalToken(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if s.internalToken == "" {
			writeError(w, http.StatusServiceUnavailable, "internal_auth_unavailable", "internal authentication is not configured")
			return
		}
		expected := sha256.Sum256([]byte("Bearer " + s.internalToken))
		candidate := sha256.Sum256([]byte(r.Header.Get("Authorization")))
		if subtle.ConstantTimeCompare(expected[:], candidate[:]) != 1 {
			writeError(w, http.StatusUnauthorized, "invalid_internal_token", "a valid internal token is required")
			return
		}
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

func writeClusterError(w http.ResponseWriter, err error) {
	switch {
	case errors.Is(err, cluster.ErrUnavailable):
		writeError(w, http.StatusServiceUnavailable, "cluster_unavailable", err.Error())
	case meta.IsNoMatchError(err):
		writeError(w, http.StatusServiceUnavailable, "deployment_api_unavailable", "ModelDeployment CRD is not installed in the cluster")
	case apierrors.IsServiceUnavailable(err), apierrors.IsTimeout(err), apierrors.IsServerTimeout(err):
		writeError(w, http.StatusServiceUnavailable, "cluster_unavailable", "Kubernetes API is unavailable")
	case apierrors.IsNotFound(err):
		writeError(w, http.StatusNotFound, "not_found", "model deployment not found")
	case apierrors.IsAlreadyExists(err):
		writeError(w, http.StatusConflict, "conflict", "model deployment already exists")
	case apierrors.IsInvalid(err), apierrors.IsBadRequest(err):
		writeError(w, http.StatusBadRequest, "invalid_request", err.Error())
	default:
		writeError(w, http.StatusBadRequest, "invalid_request", err.Error())
	}
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
