package platform

import (
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"errors"
	"io"
	"sort"
	"sync"
	"time"

	"github.com/go-playground/validator/v10"
)

var (
	ErrAlreadyExists       = errors.New("resource already exists")
	ErrIdempotencyConflict = errors.New("idempotency key was already used with a different request")
	ErrNotFound            = errors.New("resource not found")
)

// Money is expressed in the currency's smallest unit. Never use floats for billing.
type Money struct {
	Currency string `json:"currency"`
	Amount   int64  `json:"amount"`
}

type Model struct {
	ID               string `json:"id"`
	DisplayName      string `json:"display_name"`
	MaxContextTokens int64  `json:"max_context_tokens"`
	InputPerMToken   Money  `json:"input_per_million_tokens"`
	OutputPerMToken  Money  `json:"output_per_million_tokens"`
	PriceVersion     string `json:"price_version"`
}

type Project struct {
	ID       string `json:"id" validate:"required"`
	TenantID string `json:"tenant_id" validate:"required"`
	Name     string `json:"name" validate:"required"`
}

type APIKey struct {
	ID        string    `json:"id"`
	TenantID  string    `json:"tenant_id"`
	ProjectID string    `json:"project_id"`
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
}

type APIKeyRequest struct {
	ID        string `json:"id" validate:"required"`
	TenantID  string `json:"tenant_id" validate:"required"`
	ProjectID string `json:"project_id" validate:"required"`
	Name      string `json:"name" validate:"required"`
}

type IssuedAPIKey struct {
	APIKey
	Secret string `json:"secret"`
}

type apiKeyRecord struct {
	metadata   APIKey
	secretHash [sha256.Size]byte
}

type Quote struct {
	ModelID      string `json:"model_id"`
	PriceVersion string `json:"price_version"`
	Maximum      Money  `json:"maximum"`
}

type Store struct {
	mu                 sync.RWMutex
	models             map[string]Model
	projects           map[string]Project
	apiKeys            map[string]apiKeyRecord
	projectIdempotency map[string]Project
	apiKeyIdempotency  map[string]IssuedAPIKey
	validate           *validator.Validate
}

func NewStore() *Store {
	return &Store{
		models: map[string]Model{
			"xscope-demo": {
				ID:               "xscope-demo",
				DisplayName:      "XScope Demo Model",
				MaxContextTokens: 32_768,
				InputPerMToken:   Money{Currency: "CNY", Amount: 1_000},
				OutputPerMToken:  Money{Currency: "CNY", Amount: 2_000},
				PriceVersion:     "2026-09-01",
			},
		},
		projects:           make(map[string]Project),
		apiKeys:            make(map[string]apiKeyRecord),
		projectIdempotency: make(map[string]Project),
		apiKeyIdempotency:  make(map[string]IssuedAPIKey),
		validate:           validator.New(validator.WithRequiredStructEnabled()),
	}
}

func (s *Store) ListModels() []Model {
	s.mu.RLock()
	defer s.mu.RUnlock()

	models := make([]Model, 0, len(s.models))
	for _, model := range s.models {
		models = append(models, model)
	}
	sort.Slice(models, func(i, j int) bool { return models[i].ID < models[j].ID })
	return models
}

func (s *Store) ListProjects() []Project {
	s.mu.RLock()
	defer s.mu.RUnlock()

	projects := make([]Project, 0, len(s.projects))
	for _, project := range s.projects {
		projects = append(projects, project)
	}
	sort.Slice(projects, func(i, j int) bool { return projects[i].ID < projects[j].ID })
	return projects
}

func (s *Store) ListAPIKeys() []APIKey {
	s.mu.RLock()
	defer s.mu.RUnlock()

	apiKeys := make([]APIKey, 0, len(s.apiKeys))
	for _, record := range s.apiKeys {
		apiKeys = append(apiKeys, record.metadata)
	}
	sort.Slice(apiKeys, func(i, j int) bool { return apiKeys[i].ID < apiKeys[j].ID })
	return apiKeys
}

func (s *Store) PutProject(project Project) error {
	_, _, err := s.CreateProject("", project)
	return err
}

func (s *Store) CreateProject(idempotencyKey string, project Project) (Project, bool, error) {
	if err := s.validate.Struct(project); err != nil {
		return Project{}, false, errors.New("id, tenant_id and name are required")
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if idempotencyKey != "" {
		if existing, ok := s.projectIdempotency[idempotencyKey]; ok {
			if existing != project {
				return Project{}, false, ErrIdempotencyConflict
			}
			return existing, false, nil
		}
	}
	if _, exists := s.projects[project.ID]; exists {
		return Project{}, false, ErrAlreadyExists
	}
	s.projects[project.ID] = project
	if idempotencyKey != "" {
		s.projectIdempotency[idempotencyKey] = project
	}
	return project, true, nil
}

func (s *Store) IssueAPIKey(idempotencyKey string, request APIKeyRequest) (IssuedAPIKey, bool, error) {
	return s.issueAPIKey(idempotencyKey, request, rand.Reader, time.Now().UTC())
}

func (s *Store) issueAPIKey(
	idempotencyKey string,
	request APIKeyRequest,
	random io.Reader,
	createdAt time.Time,
) (IssuedAPIKey, bool, error) {
	if err := s.validate.Struct(request); err != nil {
		return IssuedAPIKey{}, false, errors.New("id, tenant_id, project_id and name are required")
	}
	secretBytes := make([]byte, 24)
	if _, err := io.ReadFull(random, secretBytes); err != nil {
		return IssuedAPIKey{}, false, errors.New("could not generate API key")
	}
	secret := "xs_" + base64.RawURLEncoding.EncodeToString(secretBytes)
	issued := IssuedAPIKey{
		APIKey: APIKey{
			ID:        request.ID,
			TenantID:  request.TenantID,
			ProjectID: request.ProjectID,
			Name:      request.Name,
			CreatedAt: createdAt,
		},
		Secret: secret,
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if existing, ok := s.apiKeyIdempotency[idempotencyKey]; ok {
		if existing.ID != request.ID || existing.TenantID != request.TenantID ||
			existing.ProjectID != request.ProjectID || existing.Name != request.Name {
			return IssuedAPIKey{}, false, ErrIdempotencyConflict
		}
		return existing, false, nil
	}
	project, ok := s.projects[request.ProjectID]
	if !ok {
		return IssuedAPIKey{}, false, errors.New("project not found")
	}
	if project.TenantID != request.TenantID {
		return IssuedAPIKey{}, false, errors.New("project does not belong to tenant")
	}
	if _, exists := s.apiKeys[request.ID]; exists {
		return IssuedAPIKey{}, false, ErrAlreadyExists
	}
	s.apiKeys[request.ID] = apiKeyRecord{
		metadata:   issued.APIKey,
		secretHash: sha256.Sum256([]byte(secret)),
	}
	s.apiKeyIdempotency[idempotencyKey] = issued
	return issued, true, nil
}

func (s *Store) VerifyAPIKey(secret string) (APIKey, bool) {
	digest := sha256.Sum256([]byte(secret))
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, record := range s.apiKeys {
		if subtle.ConstantTimeCompare(record.secretHash[:], digest[:]) == 1 {
			return record.metadata, true
		}
	}
	return APIKey{}, false
}

func (s *Store) RevokeAPIKey(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if _, exists := s.apiKeys[id]; !exists {
		return ErrNotFound
	}
	delete(s.apiKeys, id)
	for idempotencyKey, issued := range s.apiKeyIdempotency {
		if issued.ID == id {
			delete(s.apiKeyIdempotency, idempotencyKey)
		}
	}
	return nil
}

func (s *Store) Quote(modelID string, inputTokens, outputTokens int64) (Quote, error) {
	s.mu.RLock()
	model, ok := s.models[modelID]
	s.mu.RUnlock()
	if !ok {
		return Quote{}, errors.New("model not found")
	}
	if inputTokens < 0 || outputTokens < 0 {
		return Quote{}, errors.New("token counts must be non-negative")
	}

	// Round up each component so a non-zero usage cannot become free.
	inputCost := ceilDiv(inputTokens*model.InputPerMToken.Amount, 1_000_000)
	outputCost := ceilDiv(outputTokens*model.OutputPerMToken.Amount, 1_000_000)
	return Quote{
		ModelID:      model.ID,
		PriceVersion: model.PriceVersion,
		Maximum:      Money{Currency: model.InputPerMToken.Currency, Amount: inputCost + outputCost},
	}, nil
}

func ceilDiv(value, divisor int64) int64 {
	if value == 0 {
		return 0
	}
	return (value + divisor - 1) / divisor
}
