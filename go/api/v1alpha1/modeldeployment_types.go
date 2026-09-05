package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

type ModelSpec struct {
	ID       string `json:"id"`
	Revision string `json:"revision"`
	URI      string `json:"uri"`
	Checksum string `json:"checksum"`
}

type RuntimeSpec struct {
	Image     string   `json:"image"`
	Protocol  string   `json:"protocol"`
	Port      int32    `json:"port,omitempty"`
	Arguments []string `json:"arguments,omitempty"`
}

type AutoscalingSpec struct {
	MinReplicas           int32 `json:"minReplicas,omitempty"`
	MaxReplicas           int32 `json:"maxReplicas,omitempty"`
	TargetPendingRequests int32 `json:"targetPendingRequests,omitempty"`
}

type RolloutSpec struct {
	Strategy     string `json:"strategy,omitempty"`
	CanaryWeight int32  `json:"canaryWeight,omitempty"`
}

type ModelDeploymentSpec struct {
	Model        ModelSpec                   `json:"model"`
	Runtime      RuntimeSpec                 `json:"runtime"`
	Replicas     int32                       `json:"replicas"`
	Resources    corev1.ResourceRequirements `json:"resources"`
	Autoscaling  *AutoscalingSpec            `json:"autoscaling,omitempty"`
	Rollout      *RolloutSpec                `json:"rollout,omitempty"`
	NodeSelector map[string]string           `json:"nodeSelector,omitempty"`
	Tolerations  []corev1.Toleration         `json:"tolerations,omitempty"`
}

type ModelDeploymentStatus struct {
	ObservedGeneration int64              `json:"observedGeneration,omitempty"`
	ReadyReplicas      int32              `json:"readyReplicas,omitempty"`
	Endpoint           string             `json:"endpoint,omitempty"`
	Conditions         []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=mdp
// +kubebuilder:printcolumn:name="Model",type=string,JSONPath=`.spec.model.id`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
type ModelDeployment struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              ModelDeploymentSpec   `json:"spec"`
	Status            ModelDeploymentStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true
type ModelDeploymentList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []ModelDeployment `json:"items"`
}
