// Package v1alpha1 contains the platform.xscope.io API types.
package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
)

var GroupVersion = schema.GroupVersion{Group: "platform.xscope.io", Version: "v1alpha1"}

var SchemeBuilder = runtime.NewSchemeBuilder(func(scheme *runtime.Scheme) error {
	scheme.AddKnownTypes(GroupVersion, &ModelDeployment{}, &ModelDeploymentList{})
	metav1.AddToGroupVersion(scheme, GroupVersion)
	return nil
})

var AddToScheme = SchemeBuilder.AddToScheme
