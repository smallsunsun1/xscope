package cluster

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"sort"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/util/validation"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/config"

	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
)

var (
	ErrUnavailable  = errors.New("Kubernetes cluster is not configured")
	checksumPattern = regexp.MustCompile(`^sha256:[a-f0-9]{64}$`)
)

type Service struct {
	client client.Client
	reason error
}

func New() (*Service, error) {
	restConfig, err := config.GetConfig()
	if err != nil {
		return nil, err
	}
	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		return nil, err
	}
	if err := platformv1alpha1.AddToScheme(scheme); err != nil {
		return nil, err
	}
	kubeClient, err := client.New(restConfig, client.Options{Scheme: scheme})
	if err != nil {
		return nil, err
	}
	return NewWithClient(kubeClient), nil
}

func NewWithClient(kubeClient client.Client) *Service {
	return &Service{client: kubeClient}
}

func Unavailable(reason error) *Service {
	return &Service{reason: reason}
}

func (s *Service) Available() bool {
	return s != nil && s.client != nil
}

func (s *Service) Reason() string {
	if s == nil || s.reason == nil {
		return ""
	}
	return s.reason.Error()
}

func (s *Service) List(ctx context.Context, namespace string) ([]platformv1alpha1.ModelDeployment, error) {
	if !s.Available() {
		return nil, ErrUnavailable
	}
	var list platformv1alpha1.ModelDeploymentList
	if err := s.client.List(ctx, &list, client.InNamespace(namespace)); err != nil {
		return nil, err
	}
	sort.Slice(list.Items, func(i, j int) bool {
		return list.Items[i].CreationTimestamp.After(list.Items[j].CreationTimestamp.Time)
	})
	return list.Items, nil
}

func (s *Service) Create(ctx context.Context, deployment *platformv1alpha1.ModelDeployment) error {
	if !s.Available() {
		return ErrUnavailable
	}
	if err := validateDeployment(deployment); err != nil {
		return err
	}
	deployment.APIVersion = platformv1alpha1.GroupVersion.String()
	deployment.Kind = "ModelDeployment"
	deployment.Status = platformv1alpha1.ModelDeploymentStatus{}
	return s.client.Create(ctx, deployment)
}

func (s *Service) Scale(ctx context.Context, namespace, name string, replicas int32) (*platformv1alpha1.ModelDeployment, error) {
	if !s.Available() {
		return nil, ErrUnavailable
	}
	if replicas < 0 {
		return nil, errors.New("replicas must be non-negative")
	}
	deployment := &platformv1alpha1.ModelDeployment{}
	if err := s.client.Get(ctx, client.ObjectKey{Namespace: namespace, Name: name}, deployment); err != nil {
		return nil, err
	}
	deployment.Spec.Replicas = replicas
	if err := s.client.Update(ctx, deployment); err != nil {
		return nil, err
	}
	return deployment, nil
}

func (s *Service) Delete(ctx context.Context, namespace, name string) error {
	if !s.Available() {
		return ErrUnavailable
	}
	deployment := &platformv1alpha1.ModelDeployment{}
	deployment.Namespace = namespace
	deployment.Name = name
	if err := s.client.Delete(ctx, deployment); err != nil && !apierrors.IsNotFound(err) {
		return err
	}
	return nil
}

func validateDeployment(deployment *platformv1alpha1.ModelDeployment) error {
	if deployment == nil {
		return errors.New("deployment is required")
	}
	if errorsForName := validation.IsDNS1123Subdomain(deployment.Name); len(errorsForName) > 0 {
		return fmt.Errorf("invalid deployment name: %s", errorsForName[0])
	}
	if errorsForNamespace := validation.IsDNS1123Label(deployment.Namespace); len(errorsForNamespace) > 0 {
		return fmt.Errorf("invalid namespace: %s", errorsForNamespace[0])
	}
	if deployment.Spec.Model.ID == "" || deployment.Spec.Model.Revision == "" || deployment.Spec.Model.URI == "" {
		return errors.New("model id, revision and uri are required")
	}
	if !checksumPattern.MatchString(deployment.Spec.Model.Checksum) {
		return errors.New("model checksum must be sha256 followed by 64 lowercase hexadecimal characters")
	}
	if deployment.Spec.Runtime.Image == "" {
		return errors.New("runtime image is required")
	}
	switch deployment.Spec.Runtime.Protocol {
	case "openai", "triton-grpc", "custom-http":
	default:
		return errors.New("runtime protocol must be openai, triton-grpc or custom-http")
	}
	if deployment.Spec.Runtime.Port == 0 {
		deployment.Spec.Runtime.Port = 8000
	}
	if deployment.Spec.Runtime.Port < 1 || deployment.Spec.Replicas < 0 {
		return errors.New("runtime port must be positive and replicas must be non-negative")
	}
	if deployment.Spec.Rollout != nil {
		if deployment.Spec.Rollout.Strategy == "" {
			deployment.Spec.Rollout.Strategy = "rolling"
		}
		if deployment.Spec.Rollout.Strategy != "rolling" {
			return errors.New("only the rolling rollout strategy is available in v1alpha1")
		}
	}
	return nil
}
